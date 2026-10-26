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
    let sessions = delivery(None);
    let repository = Arc::new(awaken_memory_store::VolatileMemoryRepository::new(
        awaken_time::system_clock(),
    ));
    let compiler = crate::MemoryBindingCompiler::new(
        awaken_resource_contract::MemoryExecutionContent::new(repository.clone()),
        Some(Arc::new(Memory)),
    );
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
        .with_execution_scope(awaken_tenancy::ExecutionScopeRef(
            intent.workspace_id.clone().into(),
        ))
        .with_placement(awaken_dispatch_contract::PlacementRequirements::remote_required());
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
            _ => {
                changed.placement.location = awaken_dispatch_contract::ExecutionLocation::LocalOnly
            }
        }
        assert!(
            !DispatchedMemory::original_request_matches(&intent, activation.clone(), &changed),
            "M2 {cause}"
        );
    }
}
