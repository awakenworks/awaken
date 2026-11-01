use super::super::WorkerPlacedSessions;
use super::super::tests::{Memory, delivery, memory_input};
use super::*;
use crate::SessionLowering as _;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::EndCause;
use awaken_agent_contract::thread::commit::RunDisposition;
use awaken_agent_contract::thread::commit::coordinator::Coordinator as _;
use awaken_dispatch_contract::{CompletionSink as _, DispatchEnqueue as _};
use awaken_ext_memory::{MemoryExtractionExecution, MemoryExtractorSnapshot};

async fn fixture(
    scope: MemoryExtractionCommitScope,
) -> (
    WorkerPlacedSessions,
    Arc<dyn MemoryExtractionDriver>,
    Arc<awaken_memory_store::VolatileMemoryRepository>,
    MemoryExtractionIntent,
) {
    let mut sessions = delivery(None).await;
    let repository = Arc::new(awaken_memory_store::VolatileMemoryRepository::new(
        awaken_time::system_clock(),
    ));
    let compiler = crate::MemoryBindingCompiler::new(
        awaken_resource_contract::MemoryExecutionContent::new(repository.clone()),
        Some(Arc::new(Memory)),
    );
    sessions.ports.memory_bindings = compiler.clone();
    let mut input = memory_input(4);
    input.access = awaken_resource_contract::ResourceAccess::ReadWrite;
    let (_, binding) = compiler
        .compile("session", "workspace", &input, None)
        .await
        .unwrap()
        .unwrap();
    let driver = sessions.memory_extraction_driver(binding);
    let intent = MemoryExtractionIntent::new_range(
        "worker-extraction",
        "terminal",
        "workspace",
        "session",
        "terminal",
        "memory-1",
        4,
        0,
        1,
        vec![Message::text(
            MessageId("input".into()),
            Role::User,
            "Remember user preferences",
        )],
        MemoryExtractorSnapshot::host_executor("extractor", "provider", "model", "native").unwrap(),
    )
    .unwrap()
    .with_execution(MemoryExtractionExecution {
        commit_scope: scope,
        data_subject_id: Some(awaken_runtime_contract::DataSubjectId("subject".into())),
    })
    .unwrap();
    (sessions, driver, repository, intent)
}

fn proposal_messages(intent: &MemoryExtractionIntent) -> Vec<Message> {
    let mut messages = crate::memory_extraction_input(intent);
    let run = RunId(intent.auxiliary_run_id());
    messages.push(Message::new(
        MessageId::assistant(&run, 0),
        Role::Assistant,
        vec![ContentBlock::tool_use(
            "proposal",
            "write_memory",
            serde_json::json!({
                "name":"User Pref", "content":"The user prefers concise reviews."
            }),
        )],
    ));
    messages.push(Message::new(
        MessageId::tool_result_for_batch(&format!("tool-batch:{}:0", run.0), "proposal"),
        Role::Tool,
        vec![ContentBlock::tool_result(
            "proposal",
            vec![ContentBlock::text("saved")],
        )],
    ));
    messages
}

async fn commit_result(
    sessions: &WorkerPlacedSessions,
    intent: &MemoryExtractionIntent,
    disposition: RunDisposition,
    messages: Vec<Message>,
) {
    let commit = sessions
        .ports
        .reads
        .commit_for_read(&intent.auxiliary_commit_thread_id())
        .await
        .unwrap();
    commit
        .commit(awaken_agent_contract::ThreadCommit::assemble(
            ThreadId(intent.auxiliary_thread_id()),
            disposition,
            true,
            messages,
            Vec::new(),
            Vec::new(),
        ))
        .await
        .unwrap();
}

#[tokio::test]
async fn worker_memory_recovers_the_exact_completed_partition_without_dispatch() {
    // C1 legacy Session/AuxiliaryThread partition; C2 exact committed terminal
    // proposal; C3 an explicitly writable frozen binding. R1 both read only C1
    // and return one deterministic mutation,
    // without queue admission or content effects. R2 the existing content CAS
    // then applies once; exact replay retains one Memory version/receipt.
    use awaken_resource_contract::MemoryVersionQueries as _;
    for scope in [
        MemoryExtractionCommitScope::Session,
        MemoryExtractionCommitScope::AuxiliaryThread,
    ] {
        let (sessions, driver, repository, intent) = fixture(scope).await;
        commit_result(
            &sessions,
            &intent,
            RunDisposition::ended(RunId(intent.auxiliary_run_id()), EndCause::NaturalEnd),
            proposal_messages(&intent),
        )
        .await;
        let outcome = driver.extract(&intent).await;
        let MemoryExtractionOutcome::Mutations(mutations) = outcome else {
            panic!("R1: {outcome:?}");
        };
        assert_eq!(mutations.len(), 1, "R1");
        assert_eq!(mutations[0].path, "/User-Pref.md", "R1");
        assert!(
            matches!(
                sessions
                    .ports
                    .owner
                    .observe_session_run_admission(&RunId(intent.auxiliary_run_id()))
                    .await
                    .unwrap(),
                SessionRunAdmissionObservation::Missing
            ),
            "R1 no dispatch"
        );
        assert!(
            repository
                .list_versions("memory-1")
                .await
                .unwrap()
                .is_empty(),
            "R1 proposal is not an effect"
        );
        assert!(
            !driver
                .apply(&intent, &mutations[0])
                .await
                .unwrap()
                .already_applied,
            "R2"
        );
        assert!(
            driver
                .apply(&intent, &mutations[0])
                .await
                .unwrap()
                .already_applied,
            "R2"
        );
        assert_eq!(
            repository.list_versions("memory-1").await.unwrap().len(),
            1,
            "R2 one version"
        );
    }
}

#[tokio::test]
async fn worker_memory_keeps_legacy_and_started_unknown_runs_without_reentry() {
    // U1 a legacy partition has no committed Run; U2 either partition has a
    // committed Running/Indeterminate Run but no original dispatch. Every row
    // retains the original auxiliary Run in the distinct unknown outcome,
    // admits nothing and leaves content untouched. No alternate partition probe
    // or replacement Run can turn absence into nonexecution evidence.
    use awaken_resource_contract::MemoryVersionQueries as _;
    for (scope, state) in [
        (MemoryExtractionCommitScope::Session, None),
        (
            MemoryExtractionCommitScope::Session,
            Some(RunState::Running),
        ),
        (
            MemoryExtractionCommitScope::AuxiliaryThread,
            Some(RunState::Running),
        ),
        (
            MemoryExtractionCommitScope::AuxiliaryThread,
            Some(RunState::Ended(EndCause::Indeterminate)),
        ),
    ] {
        let (sessions, driver, repository, intent) = fixture(scope).await;
        if let Some(state) = state {
            let run = RunId(intent.auxiliary_run_id());
            let disposition = match state {
                RunState::Running => RunDisposition::running(run),
                RunState::Ended(cause) => RunDisposition::ended(run, cause),
                _ => unreachable!(),
            };
            commit_result(
                &sessions,
                &intent,
                disposition,
                crate::memory_extraction_input(&intent),
            )
            .await;
        }
        match driver.extract(&intent).await {
            MemoryExtractionOutcome::ExecutionIndeterminate {
                auxiliary_run_id, ..
            } => assert_eq!(auxiliary_run_id, intent.auxiliary_run_id()),
            other => panic!("U1/U2: {other:?}"),
        }
        assert!(matches!(
            sessions
                .ports
                .owner
                .observe_session_run_admission(&RunId(intent.auxiliary_run_id()))
                .await
                .unwrap(),
            SessionRunAdmissionObservation::Missing
        ));
        assert!(
            repository
                .list_versions("memory-1")
                .await
                .unwrap()
                .is_empty()
        );
    }
}

#[tokio::test]
async fn worker_memory_uses_one_ordinary_dispatch_and_its_committed_proposals() {
    // D1 fresh / already-live exact admission. The latter retains first trace
    // and publication/placement data. D2 in-memory ports finish admission in
    // their first poll; the driver remains pending at the existing completion
    // waiter. D3 commit exact output, then relay completion => one mutation,
    // no content write. The unit fixture commits through the real authority;
    // actual Worker execution remains a separate distributed acceptance gate.
    use awaken_resource_contract::MemoryVersionQueries as _;
    use std::future::Future as _;
    for replay in [false, true] {
        let (sessions, driver, repository, intent) =
            fixture(MemoryExtractionCommitScope::AuxiliaryThread).await;
        let run = RunId(intent.auxiliary_run_id());
        let thread = ThreadId(intent.auxiliary_thread_id());
        let original = if replay {
            sessions
                .ports
                .projections
                .register_thread_workspace(&thread.0, &intent.workspace_id)
                .unwrap();
            let mut activation = RunActivation::new(
                run.clone(),
                thread.clone(),
                intent.extractor.agent.clone(),
                crate::memory_extraction_input(&intent),
            );
            activation.data_subject_id = intent.execution().data_subject_id.clone();
            let request = sessions
                .ports
                .projections
                .prepare(activation)
                .fresh()
                .unwrap()
                .with_traceparent(Some(
                    "00-0123456789abcdef0123456789abcdef-0123456789abcdef-01".into(),
                ));
            sessions.ports.owner.enqueue(request.clone()).await.unwrap();
            Some(request)
        } else {
            None
        };
        let mut extraction = Box::pin(driver.extract(&intent));
        std::future::poll_fn(|context| {
            assert!(
                extraction.as_mut().poll(context).is_pending(),
                "D2 no fabricated terminal"
            );
            std::task::Poll::Ready(())
        })
        .await;
        let SessionRunAdmissionObservation::Live(queued) = sessions
            .ports
            .owner
            .observe_session_run_admission(&run)
            .await
            .unwrap()
        else {
            panic!("D1 ordinary admission missing");
        };
        assert_eq!(queued.thread_id(), &thread);
        assert_eq!(queued.session_thread_id(), &thread);
        assert_eq!(
            queued.activation.data_subject_id,
            intent.execution().data_subject_id
        );
        if let Some(original) = original {
            assert_eq!(*queued, original, "D1 first accepted request");
        }
        commit_result(
            &sessions,
            &intent,
            RunDisposition::ended(run.clone(), EndCause::NaturalEnd),
            proposal_messages(&intent),
        )
        .await;
        sessions
            .ports
            .completion
            .settled(&run, &RunState::Ended(EndCause::NaturalEnd));
        let outcome = extraction.await;
        let MemoryExtractionOutcome::Mutations(mutations) = outcome else {
            panic!("D3: {outcome:?}");
        };
        assert_eq!(mutations.len(), 1);
        assert!(
            repository
                .list_versions("memory-1")
                .await
                .unwrap()
                .is_empty(),
            "D3 controller owns content effects"
        );
    }
}

#[test]
fn worker_memory_original_request_requires_exact_activation_scope_and_partition() {
    // M1 exact immutable activation/Workspace/commit partition and strict remote
    // placement => reuse; trace is first-accepted context. M2 each conflicting
    // Run, Thread, input, subject, Workspace, physical parent, Session identity
    // scope, Session resource/runtime envelope or location => reject before
    // queue replay. No text/code parsing selects ownership.
    let intent = MemoryExtractionIntent::new_range(
        "extract",
        "terminal",
        "workspace",
        "session",
        "terminal",
        "memory",
        1,
        0,
        1,
        vec![Message::text(
            MessageId("input".into()),
            Role::User,
            "remember",
        )],
        MemoryExtractorSnapshot::host_executor("extractor", "provider", "model", "native").unwrap(),
    )
    .unwrap()
    .with_execution(MemoryExtractionExecution {
        commit_scope: MemoryExtractionCommitScope::AuxiliaryThread,
        data_subject_id: None,
    })
    .unwrap();
    let activation = RunActivation::new(
        RunId(intent.auxiliary_run_id()),
        ThreadId(intent.auxiliary_thread_id()),
        intent.extractor.agent.clone(),
        crate::memory_extraction_input(&intent),
    );
    let exact = RunDispatch::new(activation.clone())
        .with_execution_scope(awaken_agent_contract::ExecutionScopeRef(
            intent.workspace_id.clone().into(),
        ))
        .with_placement(awaken_worker_contract::PlacementRequirements::remote_required());
    assert!(
        DispatchedMemory::original_request_matches(&intent, activation.clone(), &exact),
        "M1"
    );
    for cause in [
        "run",
        "thread",
        "input",
        "subject",
        "workspace",
        "parent",
        "self-affinity",
        "session-command",
        "resources",
        "runtime",
        "location",
    ] {
        let mut changed = exact.clone();
        match cause {
            "run" => changed.activation.run_id = RunId("foreign".into()),
            "thread" => changed.activation.thread_id = ThreadId("foreign".into()),
            "input" => changed.activation.input.clear(),
            "subject" => {
                changed.activation.data_subject_id =
                    Some(awaken_runtime_contract::DataSubjectId("foreign".into()))
            }
            "workspace" => changed.execution_scope = None,
            "parent" => changed.session_thread_id = Some(ThreadId(intent.session_id.clone())),
            "self-affinity" => {
                changed.session_thread_id = Some(ThreadId(intent.auxiliary_thread_id()))
            }
            "session-command" => {
                changed.identity_scope =
                    awaken_dispatch_contract::DispatchIdentityScope::SessionCommand
            }
            "resources" => {
                changed.session_resources = Some(
                    awaken_dispatch_contract::SessionResourceEnvelope::new("workspace", "{}"),
                )
            }
            "runtime" => {
                changed.session_runtime =
                    Some(awaken_dispatch_contract::SessionRuntimeEnvelope::new("{}"))
            }
            _ => changed.placement.location = awaken_worker_contract::ExecutionLocation::LocalOnly,
        }
        assert!(
            !DispatchedMemory::original_request_matches(&intent, activation.clone(), &changed),
            "M2 {cause}"
        );
    }
}

fn parent_request(
    enabled: bool,
    writable: bool,
) -> (RunDispatch, awaken_worker_contract::RunClaim) {
    let snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder("agent")
        .model(
            awaken_runtime_contract::resolved::ModelBinding::parse("provider", "model", "native")
                .unwrap(),
        )
        .plugins([awaken_ext_memory::MEMORY_PLUGIN_ID.to_owned()])
        .plugin_config([(
            awaken_ext_memory::MEMORY_PLUGIN_ID.to_owned(),
            serde_json::json!({
                "binding_id":"memory", "recall_enabled":false, "extraction_enabled":enabled
            }),
        )])
        .build();
    let mut activation = RunActivation::new(
        RunId("parent-run".into()),
        ThreadId("logical-child".into()),
        snapshot,
        Vec::new(),
    );
    activation.data_subject_id = Some(awaken_runtime_contract::DataSubjectId("subject".into()));
    let mut input = memory_input(4);
    input.access = if writable {
        awaken_resource_contract::ResourceAccess::ReadWrite
    } else {
        awaken_resource_contract::ResourceAccess::ReadOnly
    };
    let manifest = awaken_session_configuration_contract::resource::SessionResourceManifest::new(
        "workspace",
        awaken_session_configuration_contract::resource::ResolvedSessionResources::try_new(
            vec![input],
            Vec::new(),
        )
        .unwrap(),
    );
    let request = RunDispatch::new(activation)
        .for_session(ThreadId("session".into()))
        .with_execution_scope(awaken_agent_contract::ExecutionScopeRef("workspace".into()))
        .with_session_resources(
            awaken_dispatch_execution::session_envelope::resource_envelope(&manifest).unwrap(),
        );
    let claim = awaken_worker_contract::RunClaim {
        run_id: request.run_id().clone(),
        owner: "worker".into(),
        epoch: 1,
    };
    (request, claim)
}

async fn commit_parent(sessions: &WorkerPlacedSessions, request: &RunDispatch, state: RunState) {
    let commit = sessions
        .ports
        .reads
        .commit_for_read(&request.session_thread_id().0)
        .await
        .unwrap();
    let disposition = match state {
        RunState::Ended(cause) => RunDisposition::ended(request.run_id().clone(), cause),
        RunState::Running => RunDisposition::running(request.run_id().clone()),
        RunState::Awaiting => unreachable!("this fixture tests Running or Ended authority"),
    };
    commit
        .commit(awaken_agent_contract::ThreadCommit::assemble(
            request.thread_id().clone(),
            disposition,
            true,
            vec![Message::text(
                MessageId("parent-input".into()),
                Role::User,
                "remember the preference",
            )],
            Vec::new(),
            Vec::new(),
        ))
        .await
        .unwrap();
}

#[tokio::test]
async fn worker_memory_terminal_persists_intent_before_waiting_and_replays_one_content_effect() {
    // C1 a child Run ended in its original parent partition; C2 no Worker is
    // available for its extractor. T1 before_settle returns after durable
    // enqueue/registration, retaining physical Session, logical transcript,
    // frozen attribution and AuxiliaryThread scope. It cannot wait for C2.
    // T2 commit the exact extractor proposals through the real authority,
    // relay completion and drain => one receipt/version. T3 terminal
    // redelivery => same intent/context and no second content effect.
    use awaken_resource_contract::MemoryVersionQueries as _;
    let (sessions, _, repository, _) = fixture(MemoryExtractionCommitScope::AuxiliaryThread).await;
    let (request, claim) = parent_request(true, true);
    let state = RunState::Ended(EndCause::NaturalEnd);
    commit_parent(&sessions, &request, state.clone()).await;
    let observer = sessions.worker_memory_settlement_observer();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        observer.before_settle(&request, &claim, &state, false),
    )
    .await
    .unwrap()
    .unwrap();
    let id = format!(
        "memory-extraction:{}:{}",
        request.thread_id().0,
        request.run_id().0
    );
    let outbox = sessions.ports.memory_extractions.repository();
    let intent = outbox.get_extraction(&id).await.unwrap().unwrap();
    assert_eq!(intent.session_id, "session", "T1 physical partition");
    assert_eq!(intent.logical_thread_id(), "logical-child", "T1 transcript");
    assert_eq!(
        intent.execution().commit_scope,
        MemoryExtractionCommitScope::AuxiliaryThread,
        "T1"
    );
    assert_eq!(
        intent.execution().data_subject_id,
        request.activation.data_subject_id,
        "T1"
    );
    assert_eq!(
        intent.transcript[0].id.0, "parent-input",
        "T1 committed prefix"
    );
    let background = sessions.ports.memory_extractions.background();
    assert!(
        !background.drain(std::time::Duration::ZERO).await,
        "T1 auxiliary work is still pending"
    );
    assert!(
        repository
            .list_versions("memory-1")
            .await
            .unwrap()
            .is_empty(),
        "T1 no speculative content"
    );
    commit_result(
        &sessions,
        &intent,
        RunDisposition::ended(RunId(intent.auxiliary_run_id()), EndCause::NaturalEnd),
        proposal_messages(&intent),
    )
    .await;
    sessions
        .ports
        .completion
        .settled(&RunId(intent.auxiliary_run_id()), &state);
    assert!(
        background.drain(std::time::Duration::from_secs(3)).await,
        "T2"
    );
    assert_eq!(
        outbox.get_extraction(&id).await.unwrap().unwrap().status,
        awaken_ext_memory::MemoryExtractionStatus::Completed,
        "T2"
    );
    assert_eq!(
        repository.list_versions("memory-1").await.unwrap().len(),
        1,
        "T2"
    );
    observer
        .before_settle(&request, &claim, &state, false)
        .await
        .unwrap();
    assert!(
        background.drain(std::time::Duration::from_secs(1)).await,
        "T3"
    );
    let replay = outbox.get_extraction(&id).await.unwrap().unwrap();
    assert_eq!(replay.execution(), intent.execution(), "T3 first choice");
    assert_eq!(
        replay.auxiliary_run_id(),
        intent.auxiliary_run_id(),
        "T3 stable Run"
    );
    assert_eq!(
        repository.list_versions("memory-1").await.unwrap().len(),
        1,
        "T3"
    );
}

#[tokio::test]
async fn worker_memory_terminal_selection_skips_disabled_or_proposal_work_and_refuses_unproven_truth()
 {
    // Ordered cause/effect rules: N1 disabled parent, readonly binding or an
    // existing proposal descriptor => no observer/effect, even without a
    // terminal prefix. N2 enabled+writable but absent manifest/record or
    // conflicting committed state => refuse, no intent, background task or
    // content effect. N1's descriptor rule prevents recursive extraction.
    use awaken_resource_contract::MemoryVersionQueries as _;
    for cause in [
        "disabled",
        "readonly",
        "proposal",
        "missing-manifest",
        "missing-record",
        "running-record",
    ] {
        let (sessions, _, repository, _) =
            fixture(MemoryExtractionCommitScope::AuxiliaryThread).await;
        let (mut request, claim) = parent_request(cause != "disabled", cause != "readonly");
        if cause == "proposal" {
            request
                .activation
                .snapshot
                .resolved_spec
                .tool_descriptors
                .push(awaken_ext_memory::write_memory_descriptor());
            request.activation.snapshot.recompute_fingerprint().unwrap();
            request.session_resources = None;
        } else if cause == "missing-manifest" {
            request.session_resources = None;
        } else if cause == "running-record" {
            commit_parent(&sessions, &request, RunState::Running).await;
        }
        let result = sessions
            .worker_memory_settlement_observer()
            .before_settle(
                &request,
                &claim,
                &RunState::Ended(EndCause::NaturalEnd),
                false,
            )
            .await;
        assert_eq!(
            result.is_ok(),
            matches!(cause, "disabled" | "readonly" | "proposal"),
            "N1/N2 {cause}"
        );
        assert!(
            sessions
                .ports
                .memory_extractions
                .repository()
                .recoverable_extractions(usize::MAX)
                .await
                .unwrap()
                .is_empty(),
            "N1/N2 {cause}"
        );
        assert!(
            sessions
                .ports
                .memory_extractions
                .background()
                .drain(std::time::Duration::ZERO)
                .await,
            "N1/N2 {cause}"
        );
        assert!(
            repository
                .list_versions("memory-1")
                .await
                .unwrap()
                .is_empty(),
            "N1/N2 {cause}"
        );
    }
}

#[tokio::test]
async fn worker_memory_projection_install_recovers_old_intents_without_an_enabled_parent_plugin() {
    // P1 a writable frozen binding has a durable intent and exact committed
    // extractor output; P2 the current parent's Memory extraction is disabled.
    // Installing its projection still drives P1 through the same controller:
    // one receipt/content version, with no new intent or replacement Run.
    use awaken_resource_contract::MemoryVersionQueries as _;
    use awaken_session_execution_contract::SessionInputAdmission as _;
    let (sessions, _, repository, intent) =
        fixture(MemoryExtractionCommitScope::AuxiliaryThread).await;
    sessions
        .ports
        .memory_extractions
        .controller()
        .enqueue(intent.clone())
        .await
        .unwrap();
    commit_result(
        &sessions,
        &intent,
        RunDisposition::ended(RunId(intent.auxiliary_run_id()), EndCause::NaturalEnd),
        proposal_messages(&intent),
    )
    .await;
    let (request, _) = parent_request(false, true);
    let mut projection =
        crate::test_support::projection(1, "provider/model", Some(request.activation.snapshot));
    let mut input = memory_input(4);
    input.access = awaken_resource_contract::ResourceAccess::ReadWrite;
    projection.resources =
        awaken_session_configuration_contract::resource::ResolvedSessionResources::try_new(
            vec![input],
            Vec::new(),
        )
        .unwrap();
    sessions
        .install_dispatch_projection("session", projection)
        .await
        .unwrap();
    assert!(
        sessions
            .ports
            .memory_extractions
            .background()
            .drain(std::time::Duration::from_secs(3))
            .await,
        "P1/P2"
    );
    let stored = sessions
        .ports
        .memory_extractions
        .repository()
        .get_extraction(&intent.intent_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        stored.status,
        awaken_ext_memory::MemoryExtractionStatus::Completed,
        "P1/P2"
    );
    assert_eq!(
        stored.auxiliary_run_id(),
        intent.auxiliary_run_id(),
        "P1/P2"
    );
    assert_eq!(
        repository.list_versions("memory-1").await.unwrap().len(),
        1,
        "P1/P2"
    );
}
