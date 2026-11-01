// @formal surface requirement=session.multiagent.thread_authority
// @formal reason: Host reply observation registers before the Session-owned resume is published and
// @formal reason: projects only the next committed step; coordinated Thread admission itself is
// @formal reason: awaken-session-run-delivery's.
//! Foreground observation of one Session-owned reply, over the coordinated
//! Thread admission of `awaken_session_run_delivery::CoordinatedThreads`.

#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::*;

#[cfg(test)]
use awaken_dispatch_execution::coordinated_run_activation;
#[cfg(test)]
use awaken_session_execution_contract::{
    CoordinatedRunCommand, CoordinatedRunIntent, SessionAgentReportContinuation,
};
#[cfg(test)]
use awaken_session_run_delivery::dispatch_admission_error;

impl crate::ChildRunCoordination {
    /// Register before publishing one Session-owned resume, then project only
    /// the next committed Step. The durable reply ingress and Worker execution
    /// are shared with background Managed Event reconciliation.
    pub(crate) async fn reply_and_observe_session_thread_tool(
        &self,
        delivery: awaken_session_contract::SessionThreadToolReplyDelivery,
        stream_sink: Option<std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<CommittedStepReceipt, SessionExecutionError> {
        let session_id = delivery.command.session_id.clone();
        let run_id = delivery.command.expected_run_id.clone();
        let correlation_id = delivery.command.expected_correlation_id.clone();
        self.session_runs
            .observe_published_resume(
                session_id.as_str(),
                run_id,
                correlation_id,
                stream_sink,
                async {
                    self.threads()
                        .reply_session_thread_tool(delivery)
                        .await
                        .map_err(SessionExecutionError::from)
                },
            )
            .await
    }
}

#[cfg(test)]
use awaken_dispatch_contract::{DispatchReads as _, SessionRunReservations as _};

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_dispatch_contract::DispatchClaims;
    use awaken_session_execution_contract::coordinated_activation_input;

    #[tokio::test]
    async fn dispatch_admission_errors_preserve_commit_certainty() {
        // Cause/effect graph: C1 validation proves no write; C2 an existing
        // identity has another payload; C3 storage or transport cannot prove
        // whether the write committed; C4 the original accepted request is
        // retried exactly. Effects: E1 only C1 may close a new activity; E2 C2/C3
        // remain Unavailable through Host -> Run so Session cannot mistake them
        // for proof of nonacceptance; E3 C4 reuses the original row without a
        // second queue write, claim or settlement. Decision table:
        // R1=C1 -> BadRequest/E1; R2=C2 -> Unavailable/E2;
        // R3=C3 -> Unavailable/E2; R4=C2+C4 -> success/E3.
        // R2 uses the real SQLite enqueue owner: A is committed but its first
        // acknowledgement is deliberately withheld from the retrying caller;
        // B changes only runtime input under the same Run identity. A later
        // conflict cannot erase that accepted A. Summary and operational cursor
        // stutter guard the original authority; no claim is used as a read.
        let rejected = dispatch_admission_error(awaken_dispatch_contract::DispatchError::Rejected(
            "invalid shape".into(),
        ));
        let unavailable = dispatch_admission_error(
            awaken_dispatch_contract::DispatchError::Unavailable("response lost".into()),
        );
        assert_eq!(
            rejected.kind,
            awaken_session_types::RunErrorKind::BadRequest,
            "R1/E1"
        );
        assert_eq!(
            unavailable.kind,
            awaken_session_types::RunErrorKind::Unavailable,
            "R3/E2"
        );
        assert_eq!(rejected.code, "invalid_request");
        assert_eq!(rejected.message, "invalid shape");
        assert_eq!(
            unavailable.kind,
            awaken_session_types::RunErrorKind::Unavailable
        );
        assert_eq!(unavailable.code, "unavailable");
        assert_eq!(unavailable.message, "response lost");

        use crate::host::worker_resolver::test_support::{eager_environment, test_activation};
        use awaken_dispatch_contract::{
            DispatchCursor, DispatchEnqueue as _, DispatchOperationalFeed as _, DispatchOutcome,
            DispatchSettlement as _, SessionChildAdmission, SessionRuntimeEnvelope,
        };
        use awaken_session_execution_contract::{
            CoordinatedAdmissionRef, SessionCoordinationReads as _,
        };

        let clock = Arc::new(awaken_lease_testkit::ManualClock::new(0));
        let dispatch = Arc::new(
            awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock)
                .await
                .expect("R2 original dispatch owner"),
        );
        let managed =
            crate::host::worker_resolver::test_support::adoption_host_over(dispatch.clone()).await;
        let session = awaken_session_types::SessionId::from("certainty-session");
        let parent = RunId("certainty-parent".into());
        let thread = awaken_session_execution_contract::coordinated_thread_id(
            &session,
            &parent,
            "certainty-operation",
        );
        let run = awaken_session_execution_contract::coordinated_run_id(
            &session,
            &thread,
            &parent,
            "certainty-operation",
        );
        let mut snapshot = test_activation("certainty-child", "certainty-run").snapshot;
        snapshot.metadata.source.agent_id = snapshot.root_agent_id.clone();
        snapshot.metadata.source.revision = 1;
        snapshot.recompute_fingerprint().unwrap();
        let command = CoordinatedRunCommand {
            intent: CoordinatedRunIntent::Spawn,
            session_id: session.clone(),
            thread_id: thread,
            run_id: run,
            parent_run_id: parent,
            parent_call_id: "certainty-call".into(),
            operation_id: "certainty-operation".into(),
            snapshot,
            message: "original message".into(),
            session_activity_epoch: Some(1),
            max_unarchived_threads: 24,
        };
        assert_eq!(
            managed
                .coordination
                .threads()
                .coordinated_admission_receipt(CoordinatedAdmissionRef::Child(&command))
                .await
                .unwrap(),
            None,
            "R0 actual Missing"
        );
        let mut runtime = awaken_session_configuration_contract::SessionRuntimeProjection {
            environment: eager_environment(),
            tools: None,
            mcp_stages: Some(Vec::new()),
        };
        let (encoded, requirements) = runtime.dispatch().expect("R2 frozen runtime A");
        let mut accepted = awaken_dispatch_contract::RunDispatch::new(
            coordinated_run_activation(command.clone(), "coord-root".into(), Vec::new())
                .with_model_ref_override(Some("frozen-original-route".into())),
        )
        .for_session(ThreadId("certainty-session".into()))
        .with_session_activity_epoch(1)
        .with_session_runtime(SessionRuntimeEnvelope::new(encoded));
        accepted.placement.session_mcp_credentials = Some(requirements);
        let admission = SessionChildAdmission::new(24, Vec::new());
        dispatch
            .enqueue_session_child(accepted.clone(), admission.clone())
            .await
            .expect("R2 A commits; acknowledgement is not delivered to the caller");
        let before = dispatch.list_dispatches().await.expect("R2 original row");
        assert_eq!(before.len(), 1, "R2 A was durably accepted");
        let cursor = dispatch
            .events_after(DispatchCursor::default(), 16, None)
            .await
            .expect("R2 original operation cursor")
            .next_cursor;

        // R5 axes: actual Live A permits its original business ACK despite
        // mutable fresh-input drift. Original inherited ceiling, model override
        // and resource/runtime bytes remain stored A; this ACK grants no effects
        // and does not prove the separate full-input/P0 execution obligation.
        // Changed Session/Thread/Run/parent/call /
        // message/publication/epoch cannot be projected as that accepted A.
        let receipt = managed
            .coordination
            .threads()
            .coordinated_admission_receipt(CoordinatedAdmissionRef::Child(&command))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(receipt.thread_id, command.thread_id, "R5 Live A");
        for axis in 0..9 {
            let mut changed = command.clone();
            match axis {
                0 => changed.session_id = "foreign-session".into(),
                1 => changed.thread_id = ThreadId("foreign-thread".into()),
                2 => changed.parent_run_id = RunId("foreign-parent".into()),
                3 => changed.parent_call_id = "foreign-call".into(),
                4 => changed.message = "different input".into(),
                5 => changed.snapshot.metadata.source.revision += 1,
                6 => changed.session_activity_epoch = None,
                7 => changed.session_activity_epoch = Some(0),
                _ => changed.session_activity_epoch = Some(2),
            }
            let error = managed
                .coordination
                .threads()
                .coordinated_admission_receipt(CoordinatedAdmissionRef::Child(&changed))
                .await
                .expect_err("R5 mismatch is unknown");
            assert_eq!(
                error.kind,
                awaken_session_types::RunErrorKind::Unavailable,
                "R5 axis {axis}"
            );
        }
        assert_eq!(
            dispatch.list_dispatches().await.unwrap(),
            before,
            "R5 observer stutter"
        );

        runtime.tools = Some(Default::default());
        let (encoded, requirements) = runtime.dispatch().expect("R2 frozen runtime B");
        assert_ne!(
            accepted.session_runtime.as_ref().unwrap().projection_json,
            encoded,
            "R2 tools None inherits while Some(empty) explicitly clears"
        );
        let mut changed = accepted
            .clone()
            .with_session_runtime(SessionRuntimeEnvelope::new(encoded));
        changed.placement.session_mcp_credentials = Some(requirements);
        let conflict = dispatch
            .enqueue_session_child(changed, admission.clone())
            .await
            .expect_err("R2 same Run with different runtime input must conflict");
        let message = match &conflict {
            awaken_dispatch_contract::DispatchError::Conflict(message) => message.clone(),
            other => panic!("R2 expected actual identity conflict, got {other}"),
        };
        let conflict = dispatch_admission_error(conflict);
        assert_eq!(
            conflict.kind,
            awaken_session_types::RunErrorKind::Unavailable,
            "R2/E2 the Session settler must retain the accepted activity"
        );
        assert_eq!(conflict.code, "unavailable");
        assert_eq!(
            conflict.message, message,
            "R2 original diagnostic preserved"
        );
        assert_eq!(
            dispatch.list_dispatches().await.unwrap(),
            before,
            "R2 stutter"
        );

        dispatch
            .enqueue_session_child(accepted.clone(), admission)
            .await
            .expect("R4 exact A still replays");
        assert_eq!(dispatch.list_dispatches().await.unwrap(), before, "R4/E3");
        let after = dispatch.events_after(cursor, 16, None).await.unwrap();
        assert!(after.events.is_empty(), "R2/R4 no claim or settlement");
        assert_eq!(after.next_cursor, cursor, "R2/R4 cursor stutter");
        // R6: a SessionCommand row is not an ordinary child/report admission.
        // Same original read role sees the row but cannot grant its ACK.
        let mut scoped = command.clone();
        scoped.operation_id = "reserved-root-operation".into();
        scoped.thread_id = awaken_session_execution_contract::coordinated_thread_id(
            &scoped.session_id,
            &scoped.parent_run_id,
            &scoped.operation_id,
        );
        scoped.run_id = awaken_session_execution_contract::coordinated_run_id(
            &scoped.session_id,
            &scoped.thread_id,
            &scoped.parent_run_id,
            &scoped.operation_id,
        );
        let scoped_request = awaken_dispatch_contract::RunDispatch::new(
            coordinated_run_activation(scoped.clone(), "coord-root".into(), Vec::new()),
        )
        .for_session(ThreadId(session.to_string()))
        .with_session_activity_epoch(1)
        .with_session_command_identity();
        dispatch.enqueue(scoped_request).await.unwrap();
        assert!(
            managed
                .coordination
                .threads()
                .coordinated_admission_receipt(CoordinatedAdmissionRef::Child(&scoped))
                .await
                .is_err(),
            "R6 no root-reservation ACK"
        );

        // R7: completion tombstones lack immutable body/epoch proof; never
        // fabricate an accepted receipt or turn this retained identity Missing.
        let claimed = dispatch
            .claim_run(
                accepted.run_id(),
                "completion-owner",
                1_000,
                &Default::default(),
            )
            .await
            .unwrap()
            .unwrap();
        dispatch
            .settle(
                accepted.run_id(),
                claimed.lease.epoch(),
                DispatchOutcome::Done,
                &[],
            )
            .await
            .unwrap();
        let completed = managed
            .coordination
            .threads()
            .coordinated_admission_receipt(CoordinatedAdmissionRef::Child(&command))
            .await
            .expect_err("R7 insufficient tombstone");
        assert_eq!(
            completed.kind,
            awaken_session_types::RunErrorKind::Unavailable
        );
    }

    #[test]
    fn coordinated_message_has_one_target_thread_owner_for_spawn_and_follow_up() {
        // Cause/effect graph: C1 a coordinated command starts a Thread; C2 it
        // follows up an existing Thread; C3 the exact durable operation is
        // replayed; C4 a different Run/operation is admitted. Effects: E1 C1
        // and C2 both freeze exactly one user message in the target Run
        // activation; E2 C3 reproduces the same message identity; E3 C4 gets a
        // distinct identity. Decision rules M1=(C1,C3)->E1+E2,
        // M2=(C2,C3)->E1+E2, M3=(*,C4)->E3. Spawn/follow-up is intentionally
        // absent from the identity function: both use the same Thread-owned
        // transition and neither creates PendingInput or an outbox row.
        let thread = ThreadId("agent-thread".into());
        let run = RunId("agent-run".into());
        let spawn =
            coordinated_activation_input("session", &thread, &run, "operation", "first".into());
        let follow_up_replay =
            coordinated_activation_input("session", &thread, &run, "operation", "first".into());
        let next = coordinated_activation_input(
            "session",
            &thread,
            &RunId("next-run".into()),
            "next-operation",
            "second".into(),
        );

        assert_eq!(spawn.len(), 1, "M1/E1");
        assert_eq!(spawn[0].role, Role::User, "M1/E1");
        assert_eq!(spawn[0].text_content(), "first", "M1/E1");
        assert_eq!(spawn, follow_up_replay, "M1+M2/E2");
        assert_ne!(spawn[0].id, next[0].id, "M3/E3");
    }

    #[tokio::test]
    async fn coordinated_child_admission_uses_the_canonical_dispatch_resource_generation() {
        // Cause/effect graph: C1 a prepared Session has no active manifest or a
        // stale active A; C2 its aggregate transition desires B, or is absent
        // for a legacy Session. Effects: E1 production coordinated admission
        // queues desired B before physical publication; E2 desired B wins over
        // stale A; E3 legacy admission falls back to A; E4 the queued manifest
        // owns exact Workspace scope and Worker capability; E5 selection never
        // publishes or replaces active. E6 a real File binding, exact path and
        // read-only access survive the envelope; revision-only empty fixtures
        // cannot establish the file projection that the writer needs.
        //
        // | Rule | Active | Transition | Queued | Active after | Effects    |
        // | C1   | none   | 0->B       | B      | none         | E1,E4,E5  |
        // | C2   | A      | A->B       | B      | A            | E2,E4,E5  |
        // | C3   | A      | none       | A      | A            | E3,E4,E5  |

        let manifest = |revision| {
            awaken_session_configuration_contract::resource::SessionResourceManifest::at_revision(
                "workspace-a",
                revision,
                awaken_session_configuration_contract::resource::ResolvedSessionResources::try_new(
                    vec![awaken_session_configuration_contract::resource::ResolvedInput {
                        created_at_unix_ms: None,
                        updated_at_unix_ms: None,
                        binding_id: awaken_resource_contract::BindingId::from(format!(
                            "file-binding-{revision}"
                        )),
                        source: awaken_session_configuration_contract::resource::ResolvedInputSource::File {
                            file_id: format!("file-{revision}").into(),
                        },
                        mount_path: format!("/mnt/files/confirmed-{revision}.json"),
                        access: awaken_resource_contract::ResourceAccess::ReadOnly,
                        instructions: None,
                    }],
                    Vec::new(),
                )
                .expect("File-bearing manifest"),
            )
        };
        for (rule, active_revision, desired_revision, expected_revision) in [
            ("C1", None, Some(7), 7),
            ("C2", Some(4), Some(7), 7),
            ("C3", Some(4), None, 4),
        ] {
            let session_id = format!("coordinated-resource-{rule}");
            let child_thread = ThreadId(format!("coordinated-resource-child-{rule}"));
            let child_run = RunId(format!("coordinated-resource-run-{rule}"));
            let active = active_revision.map(manifest);
            let transition = desired_revision.map(|revision| {
                awaken_session_configuration_contract::resource::SessionResourceTransition::new(
                    active.clone().unwrap_or_else(|| manifest(0)),
                    manifest(revision),
                )
                .expect("test transition belongs to one Workspace")
            });
            let baseline = awaken_session_configuration_contract::SessionBaseline::compile(
                awaken_session_configuration_contract::SessionBaselineInputs {
                    environment: awaken_session_configuration_contract::EnvironmentSnapshot {
                        environment_id: format!("coordinated-resource-environment-{rule}"),
                        revision: awaken_environment_contract::EnvironmentRevision(1),
                        self_hosted: false,
                        config_fingerprint: awaken_session_configuration_contract::EnvironmentFingerprint(
                            format!("coordinated-resource-environment-{rule}-v1"),
                        ),
                        sandbox: Default::default(),
                        sandbox_provisioning: Default::default(),
                        idle_retention: Default::default(),
                        packages: Default::default(),
                        prepared_image: None,
                        network: awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
                        credential_realization:
                            awaken_credential_contract::CredentialRealizationProfile::self_hosted_native(),
                    },
                    runtime_placement: awaken_session_types::SessionRuntimePlacement::Worker,
                    mcp_authoring: Default::default(),
                    agent_id: "agent-a".into(),
                    agent_revision: Some(1),
                    model: "model-a".into(),
                    model_override: None,
                    runtime: Some("native".into()),
                    delegate_ids: Vec::new(),
                    toolsets: Vec::new(),
                    mounts: Vec::new(),
                    env: Vec::new(),
                    prompts: Vec::new(),
                    transcript_prefix: None,
                },
            );
            let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
            let dispatch = Arc::new(
                awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock.clone())
                    .await
                    .expect("coordinated Resource dispatch store"),
            );
            let host = (crate::SessionExecutorBuilder::ephemeral(
                Arc::new(crate::host::MemoryHostModel),
                "stub",
            )
            .await
            .with_ephemeral_dispatch(dispatch.clone()))
            .build()
            .expect("validated Host dependencies");
            host.run_assembly
                .installed_sessions
                .install_with(&session_id, |installed| {
                    installed.run.session_dispatch = true;
                    installed.replace_frozen_with(|next| {
                        next.baseline = Some(baseline);
                        // This resource-generation fixture explicitly supplies the
                        // coordinator's unrestricted configuration; missing input is
                        // not evidence of an unrestricted parent.
                        next.tools = Some(Default::default());
                    });
                    installed.realization.staging_mut().manifest = active.clone();
                    installed.realization.staging_mut().transition = transition;
                })
                .expect("installed Sessions");
            let mut snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder("agent-a")
                .model(awaken_runtime_contract::resolved::ModelBinding::new(
                    "test", "model-a", "native",
                ))
                .build();
            snapshot.metadata.source.agent_id = snapshot.root_agent_id.clone();
            snapshot.metadata.source.revision = 1;
            snapshot
                .recompute_fingerprint()
                .expect("published child snapshot");

            let receipt = host
                .coordination
                .threads()
                .admit_coordinated_run(CoordinatedRunCommand {
                    intent: CoordinatedRunIntent::Spawn,
                    session_id: awaken_session_types::SessionId::from(&session_id),
                    thread_id: child_thread.clone(),
                    run_id: child_run.clone(),
                    parent_run_id: RunId(format!("coordinated-resource-parent-{rule}")),
                    parent_call_id: format!("coordinated-resource-call-{rule}"),
                    operation_id: format!("coordinated-resource-operation-{rule}"),
                    snapshot,
                    message: format!("coordinate Resource rule {rule}"),
                    session_activity_epoch: Some(1),
                    max_unarchived_threads: 24,
                })
                .await
                .unwrap_or_else(|error| panic!("{rule} production child admission: {error}"));
            assert_eq!(receipt.thread_id, child_thread, "{rule} child identity");

            clock.set(1);
            let claimed = dispatch
                .claim_run(
                    &child_run,
                    "resource-inspector",
                    30_000,
                    &Default::default(),
                )
                .await
                .unwrap_or_else(|error| panic!("{rule} inspect queued child: {error}"))
                .unwrap_or_else(|| panic!("{rule} queued child is claimable"));
            let carried = claimed
                .request
                .session_resources
                .as_ref()
                .unwrap_or_else(|| panic!("{rule} child Resource envelope"))
                .decode_with(
                    awaken_session_configuration_contract::resource::SessionResourceManifest::decode_resources,
                )
                .unwrap_or_else(|error| panic!("{rule} decode child Resource envelope: {error}"));
            assert_eq!(carried.revision, expected_revision, "{rule} generation");
            assert_eq!(
                carried,
                manifest(expected_revision),
                "{rule} exact manifest"
            );
            assert_eq!(
                claimed.request.execution_scope,
                Some(awaken_tenancy::ExecutionScopeRef(
                    awaken_tenancy::ScopeId::from("workspace-a")
                )),
                "{rule}/E4 exact scope",
            );
            assert!(
                claimed
                    .request
                    .placement
                    .required_capabilities
                    .contains(awaken_dispatch_contract::SESSION_RESOURCES_CAPABILITY),
                "{rule}/E4 eligible Worker capability",
            );
            assert_eq!(
                host.realization_executor
                    .staging()
                    .thread_resource_manifest(&session_id)
                    .expect("installed Sessions"),
                active,
                "{rule}/E5 admission must not publish active",
            );
            // W1 current unrestricted coordinator input is still explicit;
            // W2 a later projected tool update applies to the next child turn,
            // while the already-queued activation remains byte-stable. These
            // are execution-projection tests; Session CAS update admission has
            // its own existing application tests.
            let before = claimed.request.activation.clone();
            assert_eq!(
                before.inherited_toolsets().unwrap().sources(),
                &[Vec::new()],
                "W1 exact source"
            );
            let updated_tools = awaken_session_configuration_contract::SessionToolConfiguration {
                toolsets: vec![awaken_agent_contract::ToolsetPolicy {
                    source: awaken_agent_contract::ToolsetSource::Agent,
                    default: Default::default(),
                    overrides: vec![
                        awaken_agent_contract::ToolPolicyOverride::with_optional_configuration(
                            "web_fetch",
                            Default::default(),
                            Some(
                                serde_json::json!({"type":"web_fetch","domains":{"type":"allow","domains":["docs.example.com"]}}),
                            ),
                        ),
                    ],
                }],
                client_tools: Vec::new(),
            };
            host.run_assembly
                .installed_sessions
                .replace_frozen_with(&session_id, |next| next.tools = Some(updated_tools.clone()))
                .expect("installed Sessions");
            let mut target = awaken_runtime_contract::ExecutableAgentSnapshot::builder("roster-b")
                .model(awaken_runtime_contract::resolved::ModelBinding::new(
                    "test", "model-b", "native",
                ))
                .build();
            target.metadata.source.agent_id = target.root_agent_id.clone();
            target.metadata.source.revision = 1;
            target.recompute_fingerprint().unwrap();
            let own = target.resolved_spec.plugin_config.agent.toolsets.clone();
            let next_run = RunId(format!("coordinated-resource-next-{rule}"));
            host.coordination
                .threads()
                .admit_coordinated_run(CoordinatedRunCommand {
                    intent: CoordinatedRunIntent::Spawn,
                    session_id: awaken_session_types::SessionId::from(&session_id),
                    thread_id: ThreadId(format!("coordinated-resource-next-thread-{rule}")),
                    run_id: next_run.clone(),
                    parent_run_id: RunId("parent-source-next".into()),
                    parent_call_id: "next-call".into(),
                    operation_id: format!("next-source-{rule}"),
                    snapshot: target,
                    message: "next turn".into(),
                    session_activity_epoch: Some(1),
                    max_unarchived_threads: 24,
                })
                .await
                .unwrap();
            let next = dispatch
                .claim_run(&next_run, "source-inspection", 1_000, &Default::default())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                next.request
                    .activation
                    .inherited_toolsets()
                    .unwrap()
                    .sources(),
                &[updated_tools.toolsets],
                "W2 next turn gets current coordinator"
            );
            assert_eq!(
                next.request
                    .activation
                    .snapshot
                    .resolved_spec
                    .plugin_config
                    .agent
                    .toolsets,
                own,
                "W2 ID roster keeps own publication tools"
            );
            assert_eq!(
                claimed.request.activation, before,
                "W2 existing run remains frozen"
            );

            use awaken_dispatch_contract::DispatchEnqueue as _;
            use awaken_session_execution_contract::SessionCoordinationReads as _;
            let managed = host.clone();
            let baseline = host
                .run_assembly
                .installed_sessions
                .read(&session_id, |installed| installed.inputs.baseline.clone())
                .expect("installed Sessions")
                .unwrap()
                .unwrap();
            // R8 exact report Message and source Run bind the original continuation;
            // a changed rendered name/body is not guessed from current configuration.
            let report = SessionAgentReportContinuation {
                session_id: awaken_session_types::SessionId::from(&session_id),
                source_thread_id: child_thread.clone(),
                source_run_id: child_run.clone(),
                session_activity_epoch: 1,
                message: Message::text(
                    awaken_agent_contract::agent::message::Id::agent_thread_report(&child_run),
                    Role::User,
                    "Message from agent Child: original report",
                ),
            };
            let report_request = awaken_dispatch_contract::RunDispatch::new(RunActivation::new(
                report.run_id(),
                ThreadId(report.session_id.to_string()),
                before.snapshot.clone(),
                vec![report.message.clone()],
            ))
            .for_session(ThreadId(report.session_id.to_string()))
            .with_session_activity_epoch(1);
            dispatch.enqueue(report_request).await.unwrap();
            let report_rows = dispatch.list_dispatches().await.unwrap();
            assert_eq!(
                managed
                    .coordination
                    .threads()
                    .coordinated_admission_receipt(
                        awaken_session_execution_contract::CoordinatedAdmissionRef::Report(
                            &report, &baseline
                        )
                    )
                    .await
                    .unwrap()
                    .unwrap()
                    .thread_id
                    .0,
                report.session_id.as_str()
            );
            let mut changed = report.clone();
            changed.message = Message::text(
                changed.message.id.clone(),
                Role::User,
                "Message from agent Renamed: original report",
            );
            assert!(
                managed
                    .coordination
                    .threads()
                    .coordinated_admission_receipt(
                        awaken_session_execution_contract::CoordinatedAdmissionRef::Report(
                            &changed, &baseline
                        )
                    )
                    .await
                    .is_err(),
                "R8 no body normalization"
            );
            assert_eq!(
                dispatch.list_dispatches().await.unwrap(),
                report_rows,
                "R8 no effect"
            );
            let mut unknown_root = baseline.clone();
            unknown_root.agent_revision = None;
            assert!(
                managed
                    .coordination
                    .threads()
                    .coordinated_admission_receipt(
                        awaken_session_execution_contract::CoordinatedAdmissionRef::Report(
                            &report,
                            &unknown_root
                        )
                    )
                    .await
                    .is_err(),
                "R8 legacy root pin stays Unknown"
            );
            unknown_root.agent_revision = Some(2);
            assert!(
                managed
                    .coordination
                    .threads()
                    .coordinated_admission_receipt(
                        awaken_session_execution_contract::CoordinatedAdmissionRef::Report(
                            &report,
                            &unknown_root
                        )
                    )
                    .await
                    .is_err(),
                "R8 different root pin"
            );
        }
    }
}
