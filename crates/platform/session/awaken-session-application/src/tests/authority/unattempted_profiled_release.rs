use super::*;
use awaken_session_configuration_contract::resource::SessionMutationPolicy;
use awaken_session_configuration_contract::{ControlSessionCreationInputs, SessionCreationIntent};
use awaken_session_contract::{ActivationState, SessionDisposition, SessionEnvironmentState};

pub(super) fn unattempted(id: &str) -> PersistedSession {
    unattempted_with_placement(id, awaken_session_types::SessionRuntimePlacement::Local)
}

pub(in crate::tests) fn unattempted_with_placement(
    id: &str,
    placement: awaken_session_types::SessionRuntimePlacement,
) -> PersistedSession {
    let mut session = persisted(id, false, "preparing");
    let mut b = session.frozen_baseline().unwrap().clone();
    b.runtime_placement = placement;
    let compiled = creation_intent(b).finalize().unwrap();
    session.baseline =
        awaken_session_configuration_contract::SessionBaselineState::Frozen(compiled.baseline);
    session.created_at_unix_ms = Some(1);
    // A cancelled queued Run may have advanced this watermark. Empty active
    // membership, not a fabricated zero watermark, owns settled activity.
    session.activity_epoch = 2;
    session
        .resources
        .prepare(&session.session_id, file_resources("pending-file"))
        .unwrap();
    session.mcp = crate::session_mcp::SessionMcpAttachmentSet::from_initial(
        vec![crate::session_mcp::McpAttachmentDraft {
            name: "pending-mcp".into(),
            target: awaken_agent_contract::McpTarget::parse_http("https://example.test/mcp")
                .unwrap(),
            prompts_as_skills: false,
            credential: None,
            origin: crate::session_mcp::McpAttachmentOrigin::Session,
        }],
        None,
    )
    .unwrap();
    session
}

fn creation_intent(
    b: awaken_session_configuration_contract::SessionBaseline,
) -> SessionCreationIntent {
    SessionCreationIntent {
        control: ControlSessionCreationInputs {
            mutation_policy: SessionMutationPolicy::Frozen,
            application_origin: b.application_origin,
            environment: b.environment,
            runtime_placement: b.runtime_placement,
            agent_id: b.agent_id,
            agent_revision: b.agent_revision,
            inline_agent_publication: None,
            model: b.model,
            execution_model_ref: b.execution_model_ref,
            model_override: b.model_override,
            system_prompt: *b.system_prompt,
            runtime: b.runtime,
            mcp_authoring: b.mcp_authoring,
            delegate_ids: b.delegate_ids,
            toolsets: b.toolsets,
            mounts: b.mounts,
            env: b.env,
            prompts: b.prompts,
            transcript_prefix: b.transcript_prefix,
            resources: Default::default(),
            initial_mcp: Vec::new(),
        },
    }
}

fn fact(id: &str) -> ManagedLifecycleFact {
    ManagedLifecycleFact {
        id: awaken_session_contract::session_terminated_fact_id(id),
        object_id: id.into(),
        workspace_id: Some("workspace".into()),
        event_type: awaken_session_contract::SESSION_TERMINATED.into(),
        timestamp: 1,
        runtime_interval: None,
    }
}

#[test]
fn unattempted_profiled_release_checks_every_root_admission_cause() {
    run_composed_async_test(|| async {
        // Cause-effect graph: C1 Frozen WorkUnit + recorded creation; C2 Preparing
        // + no active activity/interval; C3 unmaterialized Environment + no lease
        // or attempted realization; C4 Resource/MCP participants only unattempted
        // desired intent; C5 all accepted Events settled; C6 cleanup available.
        // U1 C1&C2&C3&C4&C5&C6 -> archived + verified cleanup, no activation or
        // publication. U2 any independent !C1..C5 -> rejection, exact unchanged
        // root, zero effects. U3 !C6 -> retained archive intent, no successful
        // reply; same-command retry joins the existing cleanup (next test).
        // Conjunction short-circuiting makes one-false rules sufficient; each
        // constituent and every other execution class has its own refusal case.
        type Fault = (&'static str, fn(&mut PersistedSession));
        let faults: &[Fault] = &[
            ("managed", |s| {
                if let awaken_session_configuration_contract::SessionBaselineState::Frozen(b) =
                    &mut s.baseline
                {
                    b.mutation_policy = SessionMutationPolicy::Managed;
                }
            }),
            ("interactive", |s| {
                if let awaken_session_configuration_contract::SessionBaselineState::Frozen(b) =
                    &mut s.baseline
                {
                    b.mutation_policy = SessionMutationPolicy::FileResources;
                }
            }),
            ("missing-frozen-baseline", |s| {
                let b = s.frozen_baseline().unwrap().clone();
                s.baseline = awaken_session_configuration_contract::SessionBaselineState::Preparing(
                    creation_intent(b),
                );
            }),
            ("historical-creation", |s| s.created_at_unix_ms = None),
            ("activating", |s| {
                s.execution = SessionExecutionState::Activating
            }),
            ("running", |s| s.execution = SessionExecutionState::Running),
            ("rescheduling", |s| {
                s.execution = SessionExecutionState::Rescheduling
            }),
            ("idle", |s| s.execution = SessionExecutionState::Idle),
            ("activation-failed", |s| {
                s.execution = SessionExecutionState::ActivationFailed
            }),
            ("terminated", |s| {
                s.execution = SessionExecutionState::Terminated
            }),
            ("active-activity", |s| {
                s.active_activity_epochs.insert(2);
            }),
            ("running-interval", |s| {
                s.running_interval = Some(awaken_session_contract::SessionRuntimeIntervalStart {
                    interval_id: "old".into(),
                    activity_epoch: 2,
                    started_at_unix_ms: 1,
                    opened_revision: Default::default(),
                    admitted_after_commit_cursor: None,
                    observations: Vec::new(),
                })
            }),
            ("realization-lease", |s| {
                s.realization = Some(crate::session_resource::SessionRealizationLease {
                    owner: "old".into(),
                    runtime_incarnation: "old".into(),
                    epoch: 1,
                    expires_at_unix_ms: 10,
                })
            }),
            ("attempted-realization", |s| {
                // W5: one historical counted attempt disqualifies this root.
                let mut progress = serde_json::to_value(&*s).unwrap();
                progress["realization_progress"]["attempts"] = 1.into();
                *s = serde_json::from_value(progress).unwrap();
            }),
            ("unknown-failure", |s| {
                // W3/W5: old failure time stays unknown; do not stamp now.
                let mut progress = serde_json::to_value(&*s).unwrap();
                progress["realization_progress"]["last_error"] = "uncertain".into();
                *s = serde_json::from_value(progress).unwrap();
            }),
            ("resident-environment", |s| {
                s.environment = SessionEnvironmentState::Resident {
                    binding: "retained".into(),
                    effect_id: None,
                    generation: None,
                    idle_since_unix_ms: None,
                }
            }),
            ("active-resource", |s| {
                s.resources.active = file_resources("active-file")
            }),
            ("active-skill", |s| {
                s.resources.active = skill_resources("active-skill")
            }),
            ("resource-attempt", |s| {
                s.resources.activations[0].attempts = 1
            }),
            ("resource-state", |s| {
                s.resources.activations[0].state = ActivationState::Active
            }),
            ("resource-lease", |s| {
                s.resources.activations[0].lease_expires_at_unix_ms = Some(10)
            }),
            ("repository-retirement", |s| {
                s.resources.repository_retirements = repository_resources("repo", "retired-repo")
                    .inputs()
                    .to_vec()
            }),
            ("mcp-attempt", |s| s.mcp.attachments[0].attempts = 1),
            ("mcp-state", |s| {
                s.mcp.attachments[0].state = crate::session_mcp::McpAttachmentState::Realizing
            }),
            ("mcp-publication", |s| {
                s.mcp.attachments[0].publication_acknowledged = true
            }),
            ("mcp-claim", |s| {
                s.mcp.attachments[0].realization = Some(crate::session_mcp::McpRealizationClaim {
                    realization_id: "old".into(),
                    runtime_incarnation: "old".into(),
                    lease_epoch: 1,
                    lease_expires_at_unix_ms: 10,
                    stage_idempotency_key: "old".into(),
                })
            }),
            ("accepted-event", |s| {
                s.event_batches = session_with_unsettled_event("event").event_batches
            }),
        ];
        for (cause, fault) in faults {
            let repo = Arc::new(
                awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                    .await
                    .unwrap(),
            );
            let runtime = Arc::new(RecordingCleanupRuntime::default());
            let app = application_with_runtime(
                runtime.clone(),
                repo.clone(),
                Arc::new(RecordingEnvironmentSource::default()),
            );
            let id = format!("unattempted-{cause}");
            let mut candidate = unattempted(&id);
            fault(&mut candidate);
            create(repo.as_ref(), candidate).await;
            let before = repo.get(&SessionId::from(&id)).await.unwrap();
            assert!(
                app.termination_commands
                    .terminate_session_with_admission(
                        &SessionId::from(&id),
                        "2026-10-10T00:00:00Z",
                        fact(&id),
                        awaken_session_contract::SessionArchiveAdmission::ProfiledRelease,
                    )
                    .await
                    .is_err(),
                "U2 {cause}"
            );
            assert_eq!(
                repo.get(&SessionId::from(&id)).await.unwrap(),
                before,
                "U2 {cause}: no root write"
            );
            assert!(
                runtime.effective_ids.lock().unwrap().is_empty(),
                "U2 {cause}: no effects"
            );
        }
    });
}

#[test]
fn unattempted_profiled_release_retains_cleanup_and_absorbs_replay() {
    run_composed_async_test(|| async {
        // U1/U3: nonempty pending File/MCP intent with zero attempts must still
        // use the whole existing cleanup. Failure after archive returns no
        // receipt; exact replay completes once, without ordinary activation.
        // U4 public IdleOnly and TerminalOnly remain unchanged on Preparing.
        let repo = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .await
                .unwrap(),
        );
        let runtime = Arc::new(RecordingCleanupRuntime::default());
        let app = application_with_runtime(
            runtime.clone(),
            repo.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );
        let id = "unattempted-cleanup";
        create(repo.as_ref(), unattempted(id)).await;
        assert!(
            app.termination_commands
                .terminate_session(id, "2026-10-10T00:00:00Z", fact(id))
                .await
                .is_err(),
            "U4 public archive"
        );
        assert!(
            app.termination_commands
                .release_terminal_session(id, "2026-10-10T00:00:00Z", fact(id))
                .await
                .is_err(),
            "U4 TerminalOnly"
        );
        runtime.fail_quiesce_once.store(true, Ordering::SeqCst);
        assert!(
            app.termination_commands
                .terminate_session_with_admission(
                    &SessionId::from(id),
                    "2026-10-10T00:00:00Z",
                    fact(id),
                    awaken_session_contract::SessionArchiveAdmission::ProfiledRelease,
                )
                .await
                .is_err(),
            "U3 no premature success"
        );
        let pending = repo.get(&SessionId::from(id)).await.unwrap();
        assert!(matches!(
            pending.disposition,
            SessionDisposition::Archived { .. }
        ));
        assert!(!pending.verified_terminal_cleanup().unwrap().is_completed());
        app.termination_commands
            .terminate_session_with_admission(
                &SessionId::from(id),
                "2026-10-10T00:00:00Z",
                fact(id),
                awaken_session_contract::SessionArchiveAdmission::ProfiledRelease,
            )
            .await
            .unwrap();
        let completed = repo.get(&SessionId::from(id)).await.unwrap();
        assert!(
            completed
                .verified_terminal_cleanup()
                .unwrap()
                .is_completed(),
            "U1 whole cleanup receipt"
        );
        assert!(
            app.activities
                .begin_activity(&SessionId::from(id))
                .await
                .is_err(),
            "U1 terminal absorption"
        );
        let effects = runtime.effective_ids.lock().unwrap().len();
        app.termination_commands
            .terminate_session_with_admission(
                &SessionId::from(id),
                "another-timestamp",
                fact(id),
                awaken_session_contract::SessionArchiveAdmission::ProfiledRelease,
            )
            .await
            .unwrap();
        assert_eq!(
            repo.get(&SessionId::from(id)).await.unwrap(),
            completed,
            "U1 exact root replay"
        );
        assert_eq!(
            runtime.effective_ids.lock().unwrap().len(),
            effects,
            "U1 no duplicate cleanup"
        );
        assert_eq!(
            runtime.ordinary_resource_apply_calls.load(Ordering::SeqCst),
            0,
            "U1 no ordinary activation"
        );
    });
}

#[test]
fn unattempted_profiled_release_and_activity_share_the_root_cas() {
    run_composed_async_test(|| async {
        // U5 C2 changes concurrently after adapter observation: release and
        // activity must have one root-CAS winner. If activity wins, release
        // rechecks and rejects; if release wins, activity cannot reopen it.
        let repo = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .await
                .unwrap(),
        );
        let app = application_with_runtime(
            Arc::new(RecordingCleanupRuntime::default()),
            repo.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );
        let id = "unattempted-race";
        create(repo.as_ref(), unattempted(id)).await;
        let session_id = SessionId::from(id);
        let (activity, release) = tokio::join!(
            app.activities.begin_activity(&session_id),
            app.termination_commands.terminate_session_with_admission(
                &session_id,
                "2026-10-10T00:00:00Z",
                fact(id),
                awaken_session_contract::SessionArchiveAdmission::ProfiledRelease,
            )
        );
        assert_ne!(
            activity.is_ok(),
            release.is_ok(),
            "U5 exactly one admitted owner"
        );
        let root = repo.get(&session_id).await.unwrap();
        if release.is_ok() {
            assert!(root.is_terminal());
            assert!(!root.has_active_activities());
            assert!(root.verified_terminal_cleanup().unwrap().is_completed());
        } else {
            assert_eq!(root.execution, SessionExecutionState::Running);
            assert!(root.has_active_activities());
            assert!(!matches!(
                root.disposition,
                SessionDisposition::Archived { .. }
            ));
        }
    });
}

#[test]
fn failed_profiled_release_joins_current_run_activity_without_claiming_cleanup() {
    run_composed_async_test(|| async {
        // Cause-effect decision table: C1 Frozen primary Session; C2 committed
        // current Run failed/cancelled; C3 original activity receipt owns the
        // current epoch with no other activity; C4 no delegate/pending Events;
        // C5 cleanup driver can finish. R1 C1..C4 + !C5 => archived, original
        // cleanup retained, error response. R2 any !C1..C4 => unchanged root.
        // R3 response loss/restart of R1 => absorbing archive, no reopen and
        // no fabricated cleanup success. Successful verified cleanup itself is
        // covered by the existing preparation/terminal release matrix above.
        // Same-owner private imports retain the canonical operation-id function,
        // Session/Run arguments, receipt/Root-revision predicates, errors and IO
        // order. Inputs/behavior are unchanged: reuse R1-R4 and the existing
        // native precommit receipt table, with normal compile/lint for names.
        for cause in [
            "cancelled",
            "failed",
            "running",
            "awaiting",
            "succeeded",
            "managed",
            "missing_run",
            "missing_receipt",
            "newer_activity",
            "delegate",
            "foreign_thread",
            "race",
        ] {
            let repo = Arc::new(
                awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                    .await
                    .unwrap(),
            );
            let runtime = Arc::new(RecordingAgentAdmissionRuntime::new([]));
            let app = application_with_runtime(
                runtime.clone(),
                repo.clone(),
                Arc::new(RecordingEnvironmentSource::default()),
            );
            let id = format!("failed-profiled-{cause}");
            let sid = SessionId::from(id.as_str());
            let thread = ThreadId(id.clone());
            let run = RunId("original-run".into());
            let mut session = persisted(&id, false, "idle");
            if let awaken_session_configuration_contract::SessionBaselineState::Frozen(b) =
                &mut session.baseline
            {
                b.mutation_policy = if cause == "managed" {
                    SessionMutationPolicy::Managed
                } else {
                    SessionMutationPolicy::Frozen
                };
            }
            if cause == "delegate"
                && let awaken_session_configuration_contract::SessionBaselineState::Frozen(b) =
                    &mut session.baseline
            {
                b.delegate_ids.push("child".into());
            }
            create(repo.as_ref(), session).await;
            let operation =
                awaken_session_execution_contract::session_run_activity_operation_id(&sid, &run);
            app.activities
                .begin_activity_for_operation_with_policy(
                    &sid,
                    if cause == "missing_receipt" {
                        "unrelated-operation"
                    } else {
                        &operation
                    },
                    false,
                    None,
                    None,
                    Some((&thread, &run)),
                )
                .await
                .unwrap();
            if cause == "newer_activity" {
                app.activities
                    .begin_activity_for_operation(&sid, "newer-operation")
                    .await
                    .unwrap();
            }
            let state = match cause {
                "running" => RunState::Running,
                "awaiting" => RunState::Awaiting,
                "succeeded" => RunState::Ended(EndCause::NaturalEnd),
                "failed" => RunState::Ended(EndCause::Error(
                    awaken_agent_contract::agent::run::Failure::Inference {
                        code: "response_body".into(),
                        message: "provider failed".into(),
                    },
                )),
                _ => RunState::Ended(EndCause::Cancelled),
            };
            if cause != "missing_run" {
                runtime.boundaries.commit(&id, &thread, &run, state, "");
                if cause == "foreign_thread" {
                    let mut snapshot = runtime.boundaries.snapshot(&id, &id).unwrap();
                    snapshot.thread_id = ThreadId("foreign".into());
                    runtime
                        .boundaries
                        .0
                        .lock()
                        .unwrap()
                        .insert((id.clone(), id.clone()), snapshot);
                }
            }
            let before = repo.get(&sid).await.unwrap();
            let (release, racing_activity) = if cause == "race" {
                let (release, activity) = tokio::join!(
                    app.termination_commands.terminate_session_with_admission(
                        &sid,
                        "2026-10-14T00:00:00Z",
                        fact(&id),
                        awaken_session_contract::SessionArchiveAdmission::ProfiledRelease,
                    ),
                    app.activities
                        .begin_activity_for_operation(&sid, "racing-operation"),
                );
                (release, Some(activity))
            } else {
                (
                    app.termination_commands
                        .terminate_session_with_admission(
                            &SessionId::from(&id),
                            "2026-10-14T00:00:00Z",
                            fact(&id),
                            awaken_session_contract::SessionArchiveAdmission::ProfiledRelease,
                        )
                        .await,
                    None,
                )
            };
            assert!(release.is_err(), "no unsupported cleanup success: {cause}");
            let after = repo.get(&sid).await.unwrap();
            if let Some(activity) = racing_activity {
                // R4: one root CAS wins. Archive wins => no newer activity;
                // activity wins => old terminal Run cannot archive its epoch.
                assert_ne!(after.is_terminal(), activity.is_ok(), "R4 exclusive owner");
            } else if matches!(cause, "cancelled" | "failed") {
                assert!(after.is_terminal(), "R1 {cause}");
                assert!(
                    !after.verified_terminal_cleanup().unwrap().is_completed(),
                    "R1 retained cleanup"
                );
                assert!(
                    app.activities.begin_activity(&sid).await.is_err(),
                    "R1 no reentry"
                );
                let restarted = application_with_runtime(
                    runtime.clone(),
                    repo.clone(),
                    Arc::new(RecordingEnvironmentSource::default()),
                );
                assert!(
                    restarted
                        .termination_commands
                        .terminate_session_with_admission(
                            &SessionId::from(&id),
                            "different-display-time",
                            fact(&id),
                            awaken_session_contract::SessionArchiveAdmission::ProfiledRelease
                        )
                        .await
                        .is_err()
                );
                assert!(
                    repo.get(&sid).await.unwrap().is_terminal(),
                    "R3 restart retains fence"
                );
            } else {
                assert_eq!(after, before, "R2 {cause}");
            }
        }
    });
}
