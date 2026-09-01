//! The write-plane half of the Coordinator/Worker seam: a database-less Worker sends
//! one versioned [`CommitOperation`] under its current claim. The Coordinator cell
//! validates the claim and applies the operation through the authoritative
//! [`OperationCoordinator`].
//!
//! Paired with the dispatch transport: a worker claims a run over
//! the dispatch transport, drives it, then commits its facts here.

use std::sync::Arc;

use awaken_agent_contract::thread::commit::coordinator::OperationCoordinator as _;
use awaken_agent_contract::thread::commit::operation::{CommitOperation, CommitReceipt};
use awaken_run_ingress::{ClaimedCommitApplier, ClaimedRunCommit, DispatchQueue, RunDispatch};

use crate::host::HostError;
use crate::host::SharedHost;
struct HostCommitApplier(Arc<SharedHost>);

async fn apply_host_operation(
    host: &Arc<SharedHost>,
    trusted_dispatch: &RunDispatch,
    operation: CommitOperation,
) -> Result<CommitReceipt, HostError> {
    // Physical commit authority is Session-scoped. A child keeps its logical
    // Thread identity in `operation.commit`, while the guarded dispatch selects
    // the parent Session partition. Terminal observation deliberately does not
    // run here: the authenticated dispatch-settlement boundary is the sole
    // fallible owner that can retain the queue row until observation succeeds.
    let session_thread = trusted_dispatch.session_thread_id();
    let commit = host.commit_for_read(&session_thread.0).await?;
    commit
        .commit_operation(operation)
        .await
        .map_err(|error| HostError::internal(error.to_string()))
}

#[async_trait::async_trait]
impl ClaimedCommitApplier for HostCommitApplier {
    async fn apply(
        &self,
        trusted_dispatch: &RunDispatch,
        operation: CommitOperation,
    ) -> Result<CommitReceipt, awaken_run_ingress::ApplicationError> {
        apply_host_operation(&self.0, trusted_dispatch, operation)
            .await
            .map_err(map_host_error)
    }
}

fn map_host_error(error: HostError) -> awaken_run_ingress::ApplicationError {
    match error.kind {
        crate::HostErrorKind::BadRequest => {
            awaken_run_ingress::ApplicationError::invalid(error.message)
        }
        crate::HostErrorKind::Conflict => {
            awaken_run_ingress::ApplicationError::conflict(error.message)
        }
        crate::HostErrorKind::Internal => {
            awaken_run_ingress::ApplicationError::internal(error.message)
        }
        crate::HostErrorKind::Unavailable => {
            awaken_run_ingress::ApplicationError::unavailable(error.message)
        }
    }
}

#[must_use]
pub fn claimed_commit_service(
    dispatch: Arc<dyn DispatchQueue>,
    host: Arc<SharedHost>,
) -> awaken_run_ingress::ClaimedCommitService {
    awaken_run_ingress::ClaimedCommitService::with_applier(
        dispatch,
        Arc::new(HostCommitApplier(host)),
    )
}

pub(crate) fn remote_claimed_commit(
    upstream: &awaken_worker_transport_security::WorkerUpstream,
) -> Result<Arc<dyn ClaimedRunCommit>, HostError> {
    awaken_worker_runtime::remote_claimed_commit(upstream).map_err(HostError::internal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
    use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_agent_contract::thread::commit::RunDisposition;
    use awaken_agent_contract::thread::commit::operation::{
        CommitOperationId, commit_payload_hash,
    };
    use awaken_agent_contract::thread::commit::staged::ThreadCommit;
    use awaken_ext_memory::MemoryExtractionStatus;
    use awaken_run_ingress::{
        AnyDispatchStore, ApplicationErrorKind, ClaimedCommitRequest, Clock as _,
        MemoryDispatchStore, RunClaim, WorkerIdentity, WorkerResolver as _,
    };
    use awaken_runtime_contract::RunActivation;
    use awaken_runtime_contract::resolved::{ContextPolicy, ModelBinding, ResolvedModelCandidate};

    const MEMORY_BINDING_ID: &str = "claimed-memory-binding";
    const PRIMARY_MODEL_REF: &str = "claimed-primary-model";
    const OVERRIDE_MODEL_REF: &str = "claimed-override-model";
    const WORKSPACE_ID: &str = "claimed-memory-workspace";

    fn claimed_memory_snapshot() -> (
        awaken_runtime_contract::ExecutableAgentSnapshot,
        ResolvedModelCandidate,
        awaken_runtime_contract::ExecutableAgentSnapshot,
    ) {
        let mut snapshot = crate::config::server_config(
            "claimed-memory-agent",
            PRIMARY_MODEL_REF,
            &std::collections::HashSet::new(),
            &std::collections::HashSet::new(),
            &[awaken_ext_memory::MEMORY_PLUGIN_ID.to_string()],
            &std::collections::BTreeMap::from([(
                awaken_ext_memory::MEMORY_PLUGIN_ID.to_string(),
                serde_json::json!({
                    "binding_id": MEMORY_BINDING_ID,
                    "agent_id": "claimed-memory-extractor"
                }),
            )]),
            &[],
            ContextPolicy::KeepAll,
        );
        let override_candidate = ResolvedModelCandidate::host(ModelBinding::new(
            "claimed-override-provider",
            OVERRIDE_MODEL_REF,
            "default",
        ));
        snapshot.resolved_spec.model_candidates = vec![override_candidate.clone()];
        snapshot
            .recompute_fingerprint()
            .expect("recompute claimed Memory snapshot fingerprint");
        let extractor = awaken_ext_memory::memory_agent(
            "claimed-memory-extractor",
            override_candidate.clone(),
            awaken_ext_memory::DEFAULT_MEMORY_INSTRUCTIONS,
        );
        (snapshot, override_candidate, extractor)
    }

    fn claimed_memory_manifest(store_id: &str) -> awaken_session_contract::SessionResourceManifest {
        let config = awaken_resource_contract::MemoryStoreConfigVersion {
            memory_store_id: store_id.to_string().into(),
            version: awaken_resource_contract::ConfigVersion::INITIAL,
            retention_policy: Default::default(),
        };
        awaken_session_contract::SessionResourceManifest::at_revision(
            WORKSPACE_ID,
            7,
            awaken_session_contract::ResolvedSessionResources::try_new(
                vec![awaken_session_contract::ResolvedInput {
                    binding_id: awaken_resource_contract::BindingId::new(MEMORY_BINDING_ID),
                    source: awaken_session_contract::ResolvedInputSource::MemoryStore {
                        memory_store_id: store_id.to_string().into(),
                        config,
                    },
                    mount_path: "/memory".into(),
                    access: awaken_resource_contract::ResourceAccess::ReadWrite,
                    instructions: None,
                }],
                Vec::new(),
            )
            .expect("valid claimed Memory manifest"),
        )
    }

    fn cold_claimed_memory_host() -> (Arc<SharedHost>, crate::ManagedHost) {
        let host = Arc::new(SharedHost::new(
            Arc::new(crate::host::MemoryHostModel),
            "stub",
        ));
        let managed = crate::ManagedHost::new(host.clone())
            .with_resource_validator(crate::host::test_resource_validator())
            .install_dispatch_session_runtime();
        (host, managed)
    }

    fn assert_no_resident_session_projection(host: &SharedHost, thread: &str, rule: &str) {
        assert!(
            host.session_slots
                .read(thread, |slot| {
                    slot.runtime.is_none()
                        && !slot.environment_owner.has_local_environment()
                        && slot.workspace.is_none()
                        && slot.manifest.is_none()
                        && slot.memory_bindings.is_empty()
                        && slot.memory.is_none()
                })
                .unwrap_or(true),
            "{rule}: claimed commit must not realize a Session projection"
        );
    }

    async fn assert_completed_memory(
        host: &SharedHost,
        intent_id: &str,
        session_thread: &str,
        logical_thread: &str,
        store_id: &str,
        override_candidate: &ResolvedModelCandidate,
        rule: &str,
    ) {
        assert!(
            host.drain_runtime(std::time::Duration::from_secs(10)).await,
            "{rule}: Memory extraction did not drain"
        );
        let intent = host
            .memory
            .extraction_repository()
            .get_extraction(intent_id)
            .await
            .expect("query claimed Memory extraction")
            .expect("claimed terminal commit creates a Memory extraction");
        assert_eq!(
            intent.status,
            MemoryExtractionStatus::Completed,
            "{rule}: {:?}",
            intent.last_error
        );
        assert_eq!(intent.session_id, session_thread, "{rule}");
        assert_eq!(intent.logical_thread_id(), logical_thread, "{rule}");
        assert_eq!(intent.memory_store_id, store_id, "{rule}");
        assert_eq!(
            &intent.extractor.agent.resolved_spec.model_binding, override_candidate,
            "{rule}: extraction must retain the activation's effective model"
        );
        assert_eq!(intent.attempts, 1, "{rule}: replay is idempotent");
        assert!(
            host.memory_stores
                .fs()
                .get_by_path(store_id, "/user-prefs.md")
                .await
                .expect("query extracted Memory")
                .is_some(),
            "{rule}: extraction must write the frozen MemoryStore"
        );
        assert_eq!(
            host.memory_stores
                .fs()
                .list_versions(store_id)
                .await
                .expect("list extracted Memory versions")
                .len(),
            1,
            "{rule}: replay must not create a second Memory version"
        );
    }

    #[tokio::test]
    async fn remote_terminal_settlement_observation_is_coordinator_owned_and_idempotent() {
        // Cause/effect graph: C1 commit is nonterminal/terminal; C2 terminal operation
        // and authenticated settlement are fresh/replayed; C3 the target Coordinator
        // starts with no resident Session projection; C4 the root admission dispatch
        // carries the exact scope-matched, writable Memory binding; C5 the activation
        // selects a published fallback via `model_ref_override`. Effects: E1 committed
        // truth advances once; E2 commit alone never publishes Memory; E3 successful
        // settlement creates and completes one durable extraction and one Memory
        // version; E4 physical and logical Thread are the dispatch Thread; E5 neither
        // boundary realizes Runtime, Environment, Resource, or selected-Memory state;
        // E6 extraction freezes the activation's effective candidate. Constraints: a
        // separate admission Host invokes the production root dispatch decorator;
        // commit ingestion owns only Thread truth, while the one fallible settlement
        // observer owns post-commit Memory publication before a queue row may disappear.
        //
        // | Rule | C1          | C2     | C3   | C4    | C5       | effects          |
        // | R1   | nonterminal | fresh  | cold | exact | override | E1,E2,E4,E5      |
        // | R2   | terminal    | fresh  | cold | exact | override | E1,E2,E4,E5      |
        // | R3   | terminal    | settle | cold | exact | override | E1,E3,E4,E5,E6   |
        // | R4   | terminal    | replay | cold | exact | override | E1,E3,E4,E5,E6   |
        let thread = "remote-memory-terminal";
        let run = RunId("remote-memory-run".into());
        let store_id = "remote-memory-store";
        let manifest = claimed_memory_manifest(store_id);
        let (snapshot, override_candidate, extractor) = claimed_memory_snapshot();
        let activation =
            RunActivation::new(run.clone(), ThreadId(thread.into()), snapshot, Vec::new())
                .with_model_ref_override(Some(OVERRIDE_MODEL_REF.into()));
        let publications =
            awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([extractor.clone()])
                .expect("one exact Memory extractor publication");
        let admission = SharedHost::new(Arc::new(crate::host::MemoryHostModel), "stub")
            .with_agent_publications(Arc::new(publications));
        admission.register_thread_resource_manifest(thread, manifest);
        let dispatch = admission
            .resolved_dispatch(activation)
            .expect("production root dispatch decoration");
        assert_eq!(
            dispatch.activation.effective_model_ref(),
            OVERRIDE_MODEL_REF,
            "R1-R3/C5"
        );
        let (host, _managed) = cold_claimed_memory_host();
        assert!(
            host.session_slots.read(thread, |_| ()).is_none(),
            "R1-R3/C3 target starts cold"
        );

        let operation = |ordinal, expected_thread_version, commit: ThreadCommit| CommitOperation {
            operation_id: CommitOperationId::new(run.clone(), ordinal),
            expected_thread_version,
            payload_hash: commit_payload_hash(&commit).expect("commit hash"),
            commit,
        };
        let running = operation(
            0,
            0,
            ThreadCommit::assemble(
                ThreadId(thread.into()),
                RunDisposition::running(run.clone()),
                true,
                vec![Message::text(
                    MessageId("remote-user".into()),
                    Role::User,
                    "remember rust",
                )],
                Vec::new(),
                Vec::new(),
            ),
        );
        apply_host_operation(&host, &dispatch, running)
            .await
            .expect("R1 running commit");
        assert_no_resident_session_projection(&host, thread, "R1/E5");
        let intent_id = format!("memory-extraction:{thread}:{}", run.0);
        assert!(
            host.memory
                .extraction_repository()
                .get_extraction(&intent_id)
                .await
                .expect("R1 query")
                .is_none(),
            "R1/E3"
        );

        let terminal = operation(
            1,
            1,
            ThreadCommit::assemble(
                ThreadId(thread.into()),
                RunDisposition::ended(run.clone(), EndCause::NaturalEnd),
                true,
                vec![Message::text(
                    MessageId("remote-assistant".into()),
                    Role::Assistant,
                    "done",
                )],
                Vec::new(),
                Vec::new(),
            ),
        );
        let first = apply_host_operation(&host, &dispatch, terminal.clone())
            .await
            .expect("R2 terminal commit");
        let replay = apply_host_operation(&host, &dispatch, terminal)
            .await
            .expect("R4 terminal replay");
        assert!(!first.duplicate, "R2/E1");
        assert!(replay.duplicate, "R4/E1");
        assert!(
            host.memory
                .extraction_repository()
                .get_extraction(&intent_id)
                .await
                .expect("R2 query after commit")
                .is_none(),
            "R2/E2 commit ingestion cannot duplicate settlement observation"
        );
        let claim = RunClaim {
            run_id: run.clone(),
            owner: "remote-memory-worker".into(),
            epoch: 1,
        };
        let observer = host.worker_memory_settlement_observer();
        for rule in ["R3", "R4"] {
            observer
                .before_settle(
                    &dispatch,
                    &claim,
                    &RunState::Ended(EndCause::NaturalEnd),
                    false,
                )
                .await
                .unwrap_or_else(|error| panic!("{rule} settlement observation: {error}"));
        }
        assert_completed_memory(
            &host,
            &intent_id,
            thread,
            thread,
            store_id,
            &override_candidate,
            "R3/R4 E3,E6",
        )
        .await;
        assert_no_resident_session_projection(&host, thread, "R2-R4 E5");
    }

    #[tokio::test]
    async fn cold_durable_context_defers_terminal_delivery_to_guarded_settlement() {
        // Cause/effect graph: C1 terminal truth is committed before a Runtime is
        // resident; C2 the exact dispatch carries a writable Memory binding; C3
        // the cold context selects durable delivery; C4 guarded settlement is
        // fresh/replayed. Effects: E1 cold context construction creates no Memory
        // intent; E2 settlement creates and completes exactly one intent/version;
        // E3 replay remains idempotent. Constraint K1: DispatchWorker/HTTP
        // settlement is the sole durable terminal-delivery owner, regardless of
        // whether the context was opened from a claim. Decision rules:
        // D1=C1+C2+C3 => E1; D2=D1+C4 => E2+E3.
        let thread = "cold-durable-terminal";
        let run = RunId("cold-durable-terminal-run".into());
        let store_id = "cold-durable-terminal-memory";
        let manifest = claimed_memory_manifest(store_id);
        let (snapshot, override_candidate, extractor) = claimed_memory_snapshot();
        let activation =
            RunActivation::new(run.clone(), ThreadId(thread.into()), snapshot, Vec::new())
                .with_model_ref_override(Some(OVERRIDE_MODEL_REF.into()));
        let publications =
            awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([extractor])
                .expect("exact Memory extractor publication");
        let admission = SharedHost::new(Arc::new(crate::host::MemoryHostModel), "stub")
            .with_agent_publications(Arc::new(publications));
        admission.register_thread_resource_manifest(thread, manifest);
        let dispatch = admission
            .resolved_dispatch(activation)
            .expect("production root dispatch decoration");

        let queue =
            Arc::new(AnyDispatchStore::open_sqlite_in_memory().expect("durable dispatch store"));
        let host = Arc::new(
            SharedHost::new(Arc::new(crate::host::MemoryHostModel), "stub")
                .with_dispatch_store(queue.clone()),
        );
        crate::host::install_test_memory_mounter(&host);
        let _managed = crate::ManagedHost::new(host.clone())
            .with_resource_validator(crate::host::test_resource_validator())
            .install_dispatch_session_runtime();
        let commit = ThreadCommit::assemble(
            ThreadId(thread.into()),
            RunDisposition::ended(run.clone(), EndCause::NaturalEnd),
            true,
            vec![Message::text(
                MessageId("cold-durable-assistant".into()),
                Role::Assistant,
                "done",
            )],
            Vec::new(),
            Vec::new(),
        );
        apply_host_operation(
            &host,
            &dispatch,
            CommitOperation {
                operation_id: CommitOperationId::new(run.clone(), 0),
                expected_thread_version: 0,
                payload_hash: commit_payload_hash(&commit).expect("commit hash"),
                commit,
            },
        )
        .await
        .expect("D1 commit terminal truth");

        queue.enqueue(dispatch).await.expect("enqueue D1");
        let claimed = queue
            .claim(
                "cold-durable-worker",
                30_000,
                awaken_run_ingress::SystemClock.now_ms(),
                &Default::default(),
            )
            .await
            .expect("claim D1")
            .expect("D1 dispatch available");
        crate::host::HostWorkerResolver {
            host: Arc::downgrade(&host),
        }
        .worker_for_claimed(&claimed)
        .await
        .expect("D1 construct cold durable context");
        let ctx = host
            .session_slots
            .read(thread, |slot| slot.runtime.clone())
            .flatten()
            .expect("D1 durable context resident");
        assert!(ctx.delivery.is_durable(), "D1/C3 must use durable delivery");
        let intent_id = format!("memory-extraction:{thread}:{}", run.0);
        assert!(
            host.memory
                .extraction_repository()
                .get_extraction(&intent_id)
                .await
                .expect("D1 query")
                .is_none(),
            "D1/E1 cold durable context must not deliver terminal observation"
        );

        let observer = host.worker_memory_settlement_observer();
        for rule in ["D2-fresh", "D2-replay"] {
            observer
                .before_settle(
                    &claimed.request,
                    &RunClaim::from(&claimed.lease),
                    &RunState::Ended(EndCause::NaturalEnd),
                    false,
                )
                .await
                .unwrap_or_else(|error| panic!("{rule} settlement observation: {error}"));
        }
        assert_completed_memory(
            &host,
            &intent_id,
            thread,
            thread,
            store_id,
            &override_candidate,
            "D2/E2,E3",
        )
        .await;
    }

    #[tokio::test]
    async fn claimed_child_commit_uses_guarded_parent_partition_and_logical_child_thread() {
        // Cause/effect graph: C1 operation-id Run matches/mismatches the guarded
        // dispatch; C2 committed Run matches/mismatches it; C3 committed logical
        // Thread matches/mismatches it; C4 dispatch has/has-not a distinct parent
        // Session affinity; C5 operation is fresh/replayed; C6 the target Coordinator
        // starts cold and the child dispatch carries an exact writable Memory binding;
        // C7 the activation selects a published fallback. Effects: E1 reject before
        // any committed write; E2 append under the parent physical partition; E3
        // retain the child logical Thread/Run identity; E4 commit alone creates no
        // extraction; E5 authenticated settlement completes one extraction in the
        // frozen store using the effective candidate; E6 realize no Runtime,
        // Environment, Resource, or selected-Memory state and create no child slot or
        // physical partition; E7 exact replay is duplicate and creates no second
        // extraction attempt or Memory version. Constraints: the RunDispatch is built
        // by the production child dispatch owner and accepted only through
        // CommitEpochGuard; a rejected rule leaves Thread version zero, and only the
        // existing settlement observer consumes the same guarded dispatch afterward.
        //
        // | Rule | C1 | C2 | C3 | C4       | C5     | C6/C7 | effects          |
        // | R1   | N  | Y  | Y  | any      | any    | exact | E1               |
        // | R2   | Y  | N  | Y  | any      | any    | exact | E1               |
        // | R3   | Y  | Y  | N  | any      | any    | exact | E1               |
        // | R4   | Y  | Y  | Y  | distinct | fresh  | exact | E2,E3,E4,E6      |
        // | R5   | Y  | Y  | Y  | distinct | settle | exact | E2,E3,E5,E6      |
        // | R6   | Y  | Y  | Y  | distinct | replay | exact | E2,E3,E5,E6,E7   |
        let parent_thread = "remote-parent-session";
        let child_thread = "remote-child-thread";
        let child_run = RunId("remote-child-run".into());
        let store_id = "remote-child-memory";
        let manifest = claimed_memory_manifest(store_id);
        let (snapshot, override_candidate, extractor) = claimed_memory_snapshot();
        let activation = RunActivation::new(
            child_run.clone(),
            ThreadId(child_thread.into()),
            snapshot,
            Vec::new(),
        )
        .with_model_ref_override(Some(OVERRIDE_MODEL_REF.into()));
        let publication_source =
            awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([extractor])
                .expect("exact child auxiliary publication source");
        let request = crate::agent_runner::child_dispatch_request(
            activation,
            ThreadId(parent_thread.into()),
            Some(manifest),
            Some(&publication_source),
        )
        .expect("production child dispatch decoration");
        assert_eq!(
            request.activation.effective_model_ref(),
            OVERRIDE_MODEL_REF,
            "R1-R5/C7"
        );
        let (host, _managed) = cold_claimed_memory_host();
        assert!(
            host.session_slots.read(parent_thread, |_| ()).is_none()
                && host.session_slots.read(child_thread, |_| ()).is_none(),
            "R1-R5/C6 target starts cold"
        );
        let queue = Arc::new(MemoryDispatchStore::new());
        queue
            .enqueue(request)
            .await
            .expect("enqueue child dispatch");
        let claimed = queue
            .claim("remote-child-worker", 30_000, 0, &Default::default())
            .await
            .expect("claim child dispatch")
            .expect("child dispatch is claimable");
        let service = claimed_commit_service(queue, host.clone());
        let identity = WorkerIdentity::new("remote-child-worker", "boot", 1);

        let operation = |operation_run: RunId, commit_run: RunId, thread: &str| {
            let commit = ThreadCommit::assemble(
                ThreadId(thread.into()),
                RunDisposition::ended(commit_run, EndCause::NaturalEnd),
                true,
                vec![Message::text(
                    MessageId(format!("message-{thread}")),
                    Role::Assistant,
                    "child report",
                )],
                Vec::new(),
                Vec::new(),
            );
            CommitOperation {
                operation_id: CommitOperationId::new(operation_run, 0),
                expected_thread_version: 0,
                payload_hash: commit_payload_hash(&commit).expect("commit hash"),
                commit,
            }
        };
        let apply = |operation| ClaimedCommitRequest {
            claim: RunClaim::from(&claimed.lease),
            operation,
            identity: identity.clone(),
        };

        let wrong_operation_run = service
            .apply_claimed(apply(operation(
                RunId("other-operation-run".into()),
                child_run.clone(),
                child_thread,
            )))
            .await
            .expect_err("R1 rejects an operation-id Run mismatch");
        assert_eq!(
            wrong_operation_run.kind,
            ApplicationErrorKind::InvalidRequest
        );

        let wrong_commit_run = service
            .apply_claimed(apply(operation(
                RunId("other-commit-run".into()),
                RunId("other-commit-run".into()),
                child_thread,
            )))
            .await
            .expect_err("R2 rejects a committed Run mismatch");
        assert_eq!(wrong_commit_run.kind, ApplicationErrorKind::InvalidRequest);

        let wrong_thread = service
            .apply_claimed(apply(operation(
                child_run.clone(),
                child_run.clone(),
                "other-child-thread",
            )))
            .await
            .expect_err("R3 rejects a committed Thread mismatch");
        assert_eq!(wrong_thread.kind, ApplicationErrorKind::InvalidRequest);

        let parent_commit = host
            .commit_for_read(parent_thread)
            .await
            .expect("parent physical commit authority");
        assert!(
            parent_commit
                .authoritative_committed_messages(&ThreadId(child_thread.into()))
                .await
                .expect("R1-R3 parent query")
                .is_empty(),
            "R1-R3/E1"
        );

        let receipt = service
            .apply_claimed(apply(operation(
                child_run.clone(),
                child_run.clone(),
                child_thread,
            )))
            .await
            .expect("R4 accepts the exact guarded child commit");
        assert!(!receipt.duplicate, "R4/E2");
        let replay = service
            .apply_claimed(apply(operation(
                child_run.clone(),
                child_run.clone(),
                child_thread,
            )))
            .await
            .expect("R6 accepts the exact guarded child replay");
        assert!(replay.duplicate, "R6/E7");
        let child_messages = parent_commit
            .authoritative_committed_messages(&ThreadId(child_thread.into()))
            .await
            .expect("R4-R6 query child logical Thread through parent partition");
        assert_eq!(child_messages.len(), 1, "R4-R6 E2,E3");
        assert_eq!(child_messages[0].text_content(), "child report", "R4-R6 E3");
        assert!(
            parent_commit
                .authoritative_committed_messages(&ThreadId(parent_thread.into()))
                .await
                .expect("R4-R6 query parent logical Thread")
                .is_empty(),
            "R4-R6 E3 does not rewrite the child as the parent Thread"
        );

        let child_partition_exists = host
            .authority
            .as_ref()
            .expect("test Host has an injected authority")
            .durable_thread_exists(child_thread)
            .await
            .expect("query child physical partition");
        assert!(
            !child_partition_exists,
            "R4-R6 E6 no parallel child physical partition receives the commit"
        );
        let intent_id = format!("memory-extraction:{child_thread}:{}", child_run.0);
        assert!(
            host.memory
                .extraction_repository()
                .get_extraction(&intent_id)
                .await
                .expect("R4 query after child commit")
                .is_none(),
            "R4/E4 commit ingestion cannot publish Memory"
        );
        let observer = host.worker_memory_settlement_observer();
        for rule in ["R5", "R6"] {
            observer
                .before_settle(
                    &claimed.request,
                    &RunClaim::from(&claimed.lease),
                    &RunState::Ended(EndCause::NaturalEnd),
                    false,
                )
                .await
                .unwrap_or_else(|error| panic!("{rule} child settlement observation: {error}"));
        }
        assert_completed_memory(
            &host,
            &intent_id,
            parent_thread,
            child_thread,
            store_id,
            &override_candidate,
            "R5/R6 E5,E7",
        )
        .await;
        assert!(
            host.session_slots.read(child_thread, |_| ()).is_none(),
            "R4-R6 E6 logical child has no resident Session slot"
        );
        assert_no_resident_session_projection(&host, parent_thread, "R4-R6 E6");
    }
}
