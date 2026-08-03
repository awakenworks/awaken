//! The host Run-delegation service: run a child Agent behind the `agent_run` tool.
//!
//! This is the composition-root implementation of [`RunDelegationService`].
//! The kernel routes the delegation tool to it; here, native (in-process Agent Run)
//! and remote agents are peer backends selected only from the delegated Agent's
//! immutable publication. This module owns child-Run admission; ordinary attempt
//! routing owns the Native/ACP/A2A execution edge.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_ext_builtin_tools::AGENT_RUN;
use awaken_runtime_contract::delegation::{
    ChildRunCancellation, DelegationExecutionError, DelegationRequest, DelegationResume,
    DelegationStep, DelegationToolInput, RunDelegationService,
};
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::resolved::CatalogFingerprint;
use awaken_runtime_contract::resolver::PublishedAgentSnapshotSource;
use awaken_runtime_contract::snapshot::{AgentId, ExecutableAgentSnapshot};
#[cfg(test)]
use awaken_sandbox_local::LocalProvider;

use crate::agent_runner::{AgentRunBoundary, AgentRunSandbox, ChildRunRequest, RunScheduler};
use serde_json::Value;

#[derive(serde::Serialize)]
struct NativeDelegationContinuation {
    kind: &'static str,
    child_run_id: awaken_agent_contract::agent::run::Id,
}

/// Runs delegates behind `agent_run`: local and remote Agents share the same
/// first-class child Run identity and lifecycle; only placement differs.
#[derive(Clone)]
pub(crate) struct HostRunDelegationService {
    llm: Arc<dyn LlmExecutor>,
    /// Native children execute in the Session-owned environment. Isolation is a
    /// Session placement decision, not a per-delegation switch.
    sandbox: Arc<crate::session_environment::SessionEnvironment>,
    /// The one frozen target map used by admission and execution. Keeping the
    /// snapshot beside the edge removes current-catalog lookup from child start.
    targets: HashMap<AgentId, ResolvedDelegationTarget>,
    adapters: crate::agent_runner::ChildExecutionAdapters,
    publications: Option<Arc<dyn PublishedAgentSnapshotSource>>,
    workspace: String,
    scheduler: Option<RunScheduler>,
}

#[derive(Clone, Debug)]
struct ResolvedDelegationTarget {
    snapshot: ExecutableAgentSnapshot,
    recursive_self: bool,
}

impl HostRunDelegationService {
    pub(crate) fn new(
        llm: Arc<dyn LlmExecutor>,
        sandbox: Arc<crate::session_environment::SessionEnvironment>,
        parent_snapshot: &ExecutableAgentSnapshot,
        adapters: crate::agent_runner::ChildExecutionAdapters,
        publications: Option<Arc<dyn PublishedAgentSnapshotSource>>,
        workspace: String,
    ) -> Result<Self, DelegationExecutionError> {
        let targets = Self::resolve_targets(parent_snapshot, publications.as_deref(), &workspace)?;
        Ok(Self {
            llm,
            sandbox,
            targets,
            adapters,
            publications,
            workspace,
            scheduler: None,
        })
    }

    pub(crate) fn with_scheduler(mut self, scheduler: Option<RunScheduler>) -> Self {
        self.scheduler = scheduler;
        self
    }

    fn resolve_targets(
        parent: &ExecutableAgentSnapshot,
        publications: Option<&dyn PublishedAgentSnapshotSource>,
        workspace: &str,
    ) -> Result<HashMap<AgentId, ResolvedDelegationTarget>, DelegationExecutionError> {
        let mut targets = HashMap::new();
        for binding in &parent.resolved_spec.plugin_config.agent.delegates {
            if targets.contains_key(&binding.agent_id) {
                return Err(DelegationExecutionError::new(format!(
                    "delegate agent {:?} occurs more than once in the published targets",
                    binding.agent_id.0
                )));
            }
            let snapshot = if binding.recursive_self {
                if binding.agent_id != parent.root_agent_id {
                    return Err(DelegationExecutionError::new(
                        "recursive-self delegation target does not match its owner",
                    ));
                }
                parent.clone()
            } else {
                let source = publications.ok_or_else(|| {
                    DelegationExecutionError::new(format!(
                        "delegate agent {:?} has no publication source",
                        binding.agent_id.0
                    ))
                })?;
                binding
                    .source_revision
                    .and_then(|revision| {
                        source.at_revision(workspace, &binding.agent_id, revision)
                    })
                    .or_else(|| {
                        binding
                            .source_revision
                            .is_none()
                            .then(|| source.current(workspace, &binding.agent_id))
                            .flatten()
                    })
                    .ok_or_else(|| {
                        DelegationExecutionError::new(format!(
                            "delegate agent {:?} revision {:?} has no published executable snapshot",
                            binding.agent_id.0, binding.source_revision
                        ))
                    })?
            };
            if snapshot.root_agent_id != binding.agent_id {
                return Err(DelegationExecutionError::new(
                    "published delegate snapshot identity does not match its target",
                ));
            }
            targets.insert(
                binding.agent_id.clone(),
                ResolvedDelegationTarget {
                    snapshot,
                    recursive_self: binding.recursive_self,
                },
            );
        }
        Ok(targets)
    }

    fn exact_snapshot(
        &self,
        fingerprint: &CatalogFingerprint,
    ) -> Result<ExecutableAgentSnapshot, DelegationExecutionError> {
        self.publications
            .as_ref()
            .and_then(|source| source.exact(&self.workspace, fingerprint))
            .ok_or_else(|| {
                DelegationExecutionError::new(format!(
                    "delegated Run snapshot {:?} is unavailable",
                    fingerprint.0
                ))
            })
    }

    /// Drive a native delegate to one ordinary Run boundary.
    /// It receives that Agent's configured delegation targets; it may recursively
    /// delegate when its own config allows it. Sandbox placement is the only local
    /// execution choice made here.
    async fn native_boundary(
        &self,
        snapshot: ExecutableAgentSnapshot,
        request: ChildRunRequest,
        context: awaken_runtime_contract::runtime_context::RuntimeRunContext,
    ) -> Result<DelegationStep, DelegationExecutionError> {
        let sandbox = AgentRunSandbox::Shared(self.sandbox.as_ref());
        let child_run_id = request.run_id.clone();
        // The child keeps its own committed usage; the returned value additionally
        // rolls that usage into the initiating Run's accounting projection.
        let run_delegation = if snapshot
            .resolved_spec
            .plugin_config
            .agent
            .delegates
            .is_empty()
        {
            None
        } else {
            let mut child_service = self.clone();
            child_service.targets = Self::resolve_targets(
                &snapshot,
                child_service.publications.as_deref(),
                &child_service.workspace,
            )?;
            Some(Arc::new(child_service) as Arc<dyn RunDelegationService>)
        };
        let boundary = crate::agent_runner::run_configured_agent_until_boundary(
            &snapshot,
            sandbox,
            self.llm.clone(),
            request.run_id,
            request.origin,
            request.seed,
            request.resume,
            context,
            run_delegation,
            request.parent_thread_id,
            self.scheduler.clone(),
            self.adapters.clone(),
        )
        .await
        .map_err(|error| {
            if error.is_retryable() {
                DelegationExecutionError::retryable(error.to_string())
            } else {
                DelegationExecutionError::new(error.to_string())
            }
        })?;
        Ok(match boundary {
            AgentRunBoundary::Ended { text, usage } => DelegationStep::Ended { text, usage },
            AgentRunBoundary::Awaiting => DelegationStep::Awaiting {
                continuation: serde_json::to_value(NativeDelegationContinuation {
                    kind: "local_run",
                    child_run_id,
                })
                .map_err(|error| DelegationExecutionError::new(error.to_string()))?,
            },
        })
    }
}

#[async_trait]
impl RunDelegationService for HostRunDelegationService {
    fn tool_id(&self) -> &str {
        AGENT_RUN
    }

    fn supports_parallel_completion(&self, arguments: &Value) -> bool {
        // A child is an ordinary Run and may reach HITL. Until it settles, the
        // parent cannot promise terminal-only completion to the batch barrier.
        // Independently dispatched children regain parallelism at the RunService
        // scheduler rather than by making this false promise.
        let _ = arguments;
        false
    }

    fn allows_recursive_target(&self, agent_id: &AgentId) -> bool {
        self.targets
            .get(agent_id)
            .is_some_and(|target| target.recursive_self)
    }

    fn target_agent_id(&self, arguments: &Value) -> Result<AgentId, DelegationExecutionError> {
        Ok(DelegationToolInput::try_from(arguments)?.agent_id)
    }

    async fn start(
        &self,
        request: DelegationRequest,
    ) -> Result<DelegationStep, DelegationExecutionError> {
        let input = DelegationToolInput::try_from(&request.arguments)?;
        if input.agent_id != request.target_agent_id {
            return Err(DelegationExecutionError::new(
                "delegation target identity does not match its decoded input",
            ));
        }
        let agent_id = input.agent_id;
        let input = input.input;
        let snapshot = self
            .targets
            .get(&agent_id)
            .map(|target| target.snapshot.clone())
            .ok_or_else(|| {
                DelegationExecutionError::new(format!(
                    "delegate agent {:?} is not in this Agent's published targets",
                    agent_id.0
                ))
            })?;
        self.native_boundary(
            snapshot,
            ChildRunRequest {
                run_id: request.child_run_id,
                origin: request.origin,
                seed: Some(input.into()),
                resume: None,
                parent_thread_id: request.parent_thread_id,
            },
            request.context,
        )
        .await
    }

    async fn resume(
        &self,
        request: DelegationResume,
    ) -> Result<DelegationStep, DelegationExecutionError> {
        // The continuation identifies placement only; lifecycle authority remains
        // the child Run's committed state and ResumeTicket.
        let agent_id = request.target_agent_id;
        if !self.targets.contains_key(&agent_id) {
            return Err(DelegationExecutionError::new(format!(
                "delegate agent {:?} is not in this Agent's published targets",
                agent_id.0
            )));
        }
        let ticket = request
            .context
            .reader
            .as_ref()
            .and_then(|reader| reader.resume_ticket(&request.child_run_id))
            .ok_or_else(|| {
                DelegationExecutionError::new("delegated child Run has no resume ticket")
            })?;
        let snapshot = self.exact_snapshot(&CatalogFingerprint(ticket.catalog_fingerprint))?;
        if snapshot.root_agent_id != agent_id {
            return Err(DelegationExecutionError::new(
                "recovered delegate snapshot identity does not match its target",
            ));
        }
        self.native_boundary(
            snapshot,
            ChildRunRequest {
                run_id: request.child_run_id,
                origin: request.origin,
                seed: None,
                resume: Some(request.result),
                parent_thread_id: request.parent_thread_id,
            },
            request.context,
        )
        .await
    }

    async fn cancel(
        &self,
        cancellation: ChildRunCancellation,
    ) -> Result<(), DelegationExecutionError> {
        let Some(scheduler) = &self.scheduler else {
            // A non-durable native child shares the parent's live cancellation
            // token and has no independent queue entry to reconcile after exit.
            return Ok(());
        };
        if matches!(
            scheduler.reader.run_state(&cancellation.child_run_id),
            Some(awaken_agent_contract::agent::run::RunState::Ended(_))
        ) {
            return Ok(());
        }

        // Rebuild only the neutral cancellation service from durable authorities.
        // It first tries a live registry (there is none after restart), then removes
        // a queued/awaiting dispatch and commits the child's terminal Cancelled fact.
        // A child still leased by another process is deliberately retryable: the
        // parent's committed cancellation intent remains and reconciliation tries
        // again after that attempt reaches a boundary or loses its lease.
        let runtime = Arc::new(
            crate::config::build_runtime(self.llm.clone(), self.sandbox.as_ref())
                .with_run_delegation(Arc::new(self.clone())),
        );
        let worker = Arc::new(awaken_run_ingress::DispatchWorker::from_parts(
            runtime,
            scheduler.store.clone(),
            scheduler.commit.clone(),
            scheduler.reader.clone(),
            scheduler.owner.clone(),
        ));
        awaken_run_ingress::LiveRunControlService::new(worker)
            .cancel(&cancellation.child_run_id.0)
            .await
            .map_err(|error| DelegationExecutionError::retryable(error.to_string()))
    }
}

#[cfg(test)]
mod durable_cancel_tests {
    use super::*;
    use awaken_agent_contract::agent::delegation::DelegationOrigin;
    use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_agent_contract::thread::read::thread_reader::ThreadReader;
    use awaken_run_ingress::{AnyDispatchStore, DispatchQueue};
    use awaken_runtime_contract::agent_bindings::{AgentBindings, AgentDelegateBinding};
    use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse};
    use awaken_runtime_contract::runtime_context::RuntimeRunContext;
    use awaken_store_inmem::MemoryCommitCoordinator;
    use std::collections::HashSet;

    struct AwaitPermission;

    struct TestPublications(ExecutableAgentSnapshot);

    struct VersionedPublications {
        first: ExecutableAgentSnapshot,
        current: ExecutableAgentSnapshot,
    }

    impl PublishedAgentSnapshotSource for TestPublications {
        fn current(&self, _workspace: &str, agent_id: &AgentId) -> Option<ExecutableAgentSnapshot> {
            (self.0.root_agent_id == *agent_id).then(|| self.0.clone())
        }

        fn exact(
            &self,
            _workspace: &str,
            fingerprint: &CatalogFingerprint,
        ) -> Option<ExecutableAgentSnapshot> {
            (self.0.fingerprint == *fingerprint).then(|| self.0.clone())
        }
    }

    impl PublishedAgentSnapshotSource for VersionedPublications {
        fn current(&self, _workspace: &str, agent_id: &AgentId) -> Option<ExecutableAgentSnapshot> {
            (self.current.root_agent_id == *agent_id).then(|| self.current.clone())
        }

        fn exact(
            &self,
            _workspace: &str,
            fingerprint: &CatalogFingerprint,
        ) -> Option<ExecutableAgentSnapshot> {
            [&self.first, &self.current]
                .into_iter()
                .find(|snapshot| snapshot.fingerprint == *fingerprint)
                .cloned()
        }

        fn at_revision(
            &self,
            _workspace: &str,
            agent_id: &AgentId,
            source_revision: u64,
        ) -> Option<ExecutableAgentSnapshot> {
            (source_revision == 1 && self.first.root_agent_id == *agent_id)
                .then(|| self.first.clone())
        }
    }

    #[test]
    fn delegation_target_resolution_freezes_exact_or_current_once() {
        // Cause graph: C1=edge has an exact revision; C2=that publication exists;
        // C3=edge intentionally omits a revision. C1+C2 -> E1 freeze exact even
        // when current is newer; C1+!C2 -> E2 reject setup; !C1+C3 -> E3 resolve
        // current once and freeze it in the target map; C4=duplicate target ->
        // E4 reject rather than silently selecting one edge.
        //
        // | Rule | revision | publication | effect |
        // | V1 | 1 | v1 exists, current=v2 | freeze v1 |
        // | V2 | 99 | absent | fail before child start |
        // | V3 | none | current=v2 | freeze v2 once |
        // | V4 | duplicate | either | fail before child start |
        let first = ExecutableAgentSnapshot::builder("worker")
            .instructions("version one")
            .fingerprint("worker-v1")
            .build();
        let current = ExecutableAgentSnapshot::builder("worker")
            .instructions("version two")
            .fingerprint("worker-v2")
            .build();
        let source = VersionedPublications {
            first: first.clone(),
            current: current.clone(),
        };
        let parent = |source_revision| {
            ExecutableAgentSnapshot::builder("coordinator")
                .agent_bindings(AgentBindings {
                    delegates: vec![AgentDelegateBinding {
                        agent_id: AgentId("worker".into()),
                        source_revision,
                        recursive_self: false,
                    }],
                    ..Default::default()
                })
                .build()
        };

        let exact =
            HostRunDelegationService::resolve_targets(&parent(Some(1)), Some(&source), "workspace")
                .expect("V1");
        assert_eq!(exact[&AgentId("worker".into())].snapshot, first, "V1/E1");

        let error = HostRunDelegationService::resolve_targets(
            &parent(Some(99)),
            Some(&source),
            "workspace",
        )
        .expect_err("V2 missing exact publication must fail");
        assert!(error.to_string().contains("Some(99)"), "V2/E2");

        let resolved_current =
            HostRunDelegationService::resolve_targets(&parent(None), Some(&source), "workspace")
                .expect("V3");
        assert_eq!(
            resolved_current[&AgentId("worker".into())].snapshot,
            current,
            "V3/E3"
        );

        let mut duplicate = parent(Some(1));
        let repeated = duplicate.resolved_spec.plugin_config.agent.delegates[0].clone();
        duplicate
            .resolved_spec
            .plugin_config
            .agent
            .delegates
            .push(repeated);
        let error =
            HostRunDelegationService::resolve_targets(&duplicate, Some(&source), "workspace")
                .expect_err("V4 duplicate target must fail");
        assert!(error.to_string().contains("more than once"), "V4/E4");
    }

    #[async_trait]
    impl LlmExecutor for AwaitPermission {
        async fn infer(
            &self,
            _request: ChatRequest,
        ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
            Ok(ChatResponse {
                output: AssistantOutput::from_tool_calls(vec![
                    awaken_runtime_contract::llm::ToolCall {
                        call_id: "child-write".into(),
                        tool_id: "write".into(),
                        arguments: serde_json::json!({"path": "child.txt", "content": "x"}),
                    },
                ]),
                usage: None,
                stop_reason: None,
            })
        }
    }

    #[tokio::test]
    async fn replacement_process_cancels_an_awaiting_native_child_idempotently() {
        let root = tempfile::tempdir().expect("sandbox root");
        let provider = Arc::new(LocalProvider::new(root.path()));
        let sandbox = Arc::new(crate::session_environment::SessionEnvironment::workdir(
            provider
                .create_sandbox(&crate::provisioning::agent_run_sandbox_spec("parent"))
                .await
                .expect("parent sandbox"),
        ));
        let store = Arc::new(
            AnyDispatchStore::open_sqlite_in_memory().expect("durable child dispatch store"),
        );
        let commit = Arc::new(MemoryCommitCoordinator::new());
        let scheduler = RunScheduler {
            store: store.clone(),
            commit: commit.clone(),
            reader: commit.clone(),
            owner: "child-owner".to_string(),
            claimed_commit: None,
            recovery_projection: None,
            session_resources: None,
        };
        let child_snapshot = crate::config::server_config(
            "researcher",
            "stub",
            &HashSet::new(),
            &HashSet::new(),
            &[],
            &Default::default(),
            &[],
            awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
        );
        let parent_snapshot = crate::config::server_config(
            "assistant",
            "stub",
            &HashSet::new(),
            &HashSet::from(["researcher".to_string()]),
            &[],
            &Default::default(),
            &[],
            awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
        );
        let service = HostRunDelegationService::new(
            Arc::new(AwaitPermission),
            sandbox,
            &parent_snapshot,
            crate::agent_runner::ChildExecutionAdapters::default(),
            Some(Arc::new(TestPublications(child_snapshot))),
            "default".into(),
        )
        .expect("resolve test roster")
        .with_scheduler(Some(scheduler));
        let origin = DelegationOrigin::root_for_agent(
            RunId("parent-run".into()),
            "delegate-call",
            "parent-agent",
        );
        let child_run_id = origin.child_run_id();
        let step = service
            .start(DelegationRequest {
                child_run_id: child_run_id.clone(),
                parent_thread_id: ThreadId("parent-thread".into()),
                context: RuntimeRunContext::new()
                    .with_commit(commit.clone())
                    .with_reader(commit.clone()),
                origin: origin.clone(),
                target_agent_id: AgentId("researcher".into()),
                arguments: serde_json::json!({
                    "agent_id": "researcher",
                    "input": "write a file"
                }),
            })
            .await
            .expect("child reaches permission boundary");
        assert!(matches!(step, DelegationStep::Awaiting { .. }));
        assert_eq!(
            store.list_dispatches().await.expect("dispatch list").len(),
            1
        );

        let cancellation = ChildRunCancellation {
            delegation_id: origin.delegation_id,
            child_run_id: child_run_id.clone(),
            target_agent_id: "researcher".to_string(),
            execution_reference: None,
        };
        service
            .cancel(cancellation.clone())
            .await
            .expect("replacement cancels awaiting child");
        assert_eq!(
            commit.run_state(&child_run_id),
            Some(RunState::Ended(EndCause::Cancelled))
        );
        assert!(
            store
                .list_dispatches()
                .await
                .expect("dispatch list")
                .is_empty()
        );

        // A later process has no in-memory delivery receipt and retries the
        // durable cancellation intent. Terminal committed truth makes it a no-op.
        service
            .clone()
            .cancel(cancellation)
            .await
            .expect("duplicate cancellation is idempotent");
        assert_eq!(
            commit.run_state(&child_run_id),
            Some(RunState::Ended(EndCause::Cancelled))
        );
    }
}
