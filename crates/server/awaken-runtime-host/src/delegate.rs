//! The host Run-delegation service: run a child Agent behind the `agent_run` tool.
//!
//! This is the composition-root implementation of [`RunDelegationService`].
//! The kernel routes the delegation tool to it; here, native (in-process Agent Run)
//! and remote agents are *peer* implementations chosen by `agent_id`. A remote agent
//! is reached through the neutral [`RemoteAgent`] interface, so the wire (message shape,
//! task polling, discovery card) lives entirely in the adapter that implements it
//! (e.g. `awaken-run-executor-a2a`); this module owns only the *dispatch* — routing an
//! `agent_id` to its native or remote Agent execution — and names no protocol.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use awaken_ext_builtin_tools::AGENT_RUN;
use awaken_runtime_contract::delegation::{
    ChildRunCancellation, DelegationExecutionError, DelegationRequest, DelegationResume,
    DelegationStep, RemoteAgent, RunDelegationService,
};
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::resume::ResumeResult;
use awaken_sandbox_local::{LocalProvider, LocalSandbox};

use crate::agent_runner::{AgentRunBoundary, AgentRunSandbox, ChildRunRequest, RunScheduler};
use serde_json::Value;

use crate::host::{HostError, SharedHost};

/// The host's delegate agents (local + A2A-remote), behind one type owning their
/// shared invariant: `remotes` (agents fulfilled over A2A) is a *subset* of `ids`
/// (every advertised delegate). [`Self::add_remote`] maintains it by registering into
/// both; [`Self::native_ids`] derives the local delegates as `ids − remotes`. Keeping
/// the pair split let a caller register a remote delegate without also advertising
/// the delegate, silently breaking the `multiagent` roster.
///
/// `remotes` holds the neutral [`RemoteAgent`] interface, not a wire type: the A2A
/// transport + poll loop live in the adapter that implements it (Phase 2), so host
/// state names no protocol.
#[derive(Clone, Default)]
pub(crate) struct Delegates {
    /// All delegate agent ids (advertised as the agent's `multiagent` roster).
    ids: HashSet<String>,
    /// The subset fulfilled by a remote peer (agent id → the neutral delegate port)
    /// instead of a local Agent Run. Invariant: every key is also in `ids`.
    remotes: HashMap<String, Arc<dyn RemoteAgent>>,
    /// Per-Agent delegation rosters. Absence means that Agent has no delegation
    /// capability; being initiated by another Agent never implicitly copies the
    /// delegation_origin's roster.
    agent_rosters: HashMap<String, HashSet<String>>,
}

impl Delegates {
    /// An empty roster.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Add local delegate Agents (fulfilled by an in-process child Run).
    pub(crate) fn add_local(&mut self, ids: HashSet<String>) {
        self.ids.extend(ids);
    }

    /// Register a remote delegate: advertised in the roster AND routed to its neutral
    /// [`RemoteAgent`] interface. Maintains the `remotes ⊆ ids` invariant via both.
    pub(crate) fn add_remote(&mut self, agent_id: String, delegate: Arc<dyn RemoteAgent>) {
        self.ids.insert(agent_id.clone());
        self.remotes.insert(agent_id, delegate);
    }

    /// Configure the delegation targets available when `agent_id` itself runs.
    /// This is independent from whether `agent_id` appears in the root Agent's
    /// roster: each Agent owns its ordinary capability set.
    pub(crate) fn set_agent_roster(&mut self, agent_id: String, targets: HashSet<String>) {
        self.agent_rosters.insert(agent_id, targets);
    }

    /// Whether the roster is empty (no delegates configured at all).
    pub(crate) fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// All delegate ids (the `multiagent` advertisement).
    pub(crate) fn ids(&self) -> Vec<String> {
        self.ids.iter().cloned().collect()
    }

    /// The advertised id set, for callers that pass it by reference (config build).
    pub(crate) fn ids_set(&self) -> &HashSet<String> {
        &self.ids
    }

    /// The remote-delegate port for `agent_id`, or `None` if it is local/unknown.
    pub(crate) fn remote(&self, agent_id: &str) -> Option<&Arc<dyn RemoteAgent>> {
        self.remotes.get(agent_id)
    }
}

/// The `(agent_id, input)` a delegate `agent_run` call carries.
fn delegate_args(arguments: &Value) -> (String, String) {
    let field = |key: &str| {
        arguments
            .get(key)
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string()
    };
    (field("agent_id"), field("input"))
}

/// Runs delegates behind `agent_run`: local and remote Agents share the same
/// first-class child Run identity and lifecycle; only placement differs.
#[derive(Clone)]
pub(crate) struct HostRunDelegationService {
    llm: Arc<dyn LlmExecutor>,
    model_ref: String,
    provider: Arc<LocalProvider>,
    /// The parent agent's sandbox, shared with a native delegate by default so the two
    /// collaborate in one workspace (`默认共用`); bypassed when `reuse_sandbox` is off.
    sandbox: Arc<LocalSandbox>,
    /// Whether a native delegate reuses the parent sandbox (default) or gets a fresh,
    /// isolated one. Sandbox placement does not change child Run semantics.
    reuse_sandbox: bool,
    /// Delegates this Agent may call, regardless of local/remote placement.
    root_roster: HashSet<String>,
    /// All configured Agent identities, per-Agent rosters, and remote adapters.
    /// A child receives its own roster, never an implicit copy of its delegation_origin's.
    delegates: Delegates,
    scheduler: Option<RunScheduler>,
}

impl HostRunDelegationService {
    pub(crate) fn new(
        llm: Arc<dyn LlmExecutor>,
        model_ref: String,
        provider: Arc<LocalProvider>,
        sandbox: Arc<LocalSandbox>,
        reuse_sandbox: bool,
        delegates: Delegates,
        scheduler: Option<RunScheduler>,
    ) -> Self {
        let root_roster = delegates.ids.clone();
        Self {
            llm,
            model_ref,
            provider,
            sandbox,
            reuse_sandbox,
            root_roster,
            delegates,
            scheduler,
        }
    }

    fn may_delegate_to(
        &self,
        origin: &awaken_agent_contract::agent::delegation::DelegationOrigin,
        target: &str,
    ) -> bool {
        origin
            .agent_lineage
            .last()
            .and_then(|agent| self.delegates.agent_rosters.get(agent))
            .unwrap_or(&self.root_roster)
            .contains(target)
    }

    /// Drive a native delegate to one ordinary Run boundary.
    /// It receives that Agent's configured delegation roster; it may recursively
    /// delegate when its own config allows it. Sandbox placement is the only local
    /// execution choice made here.
    async fn native_boundary(
        &self,
        agent_id: &str,
        request: ChildRunRequest,
        context: awaken_runtime_contract::runtime_context::RuntimeRunContext,
    ) -> Result<DelegationStep, DelegationExecutionError> {
        let sandbox = if self.reuse_sandbox {
            AgentRunSandbox::Shared(&self.sandbox)
        } else {
            AgentRunSandbox::Fresh(&self.provider)
        };
        let child_run_id = request.run_id.clone();
        // The child keeps its own committed usage; the returned value additionally
        // rolls that usage into the initiating Run's accounting projection.
        let delegates = self
            .delegates
            .agent_rosters
            .get(agent_id)
            .cloned()
            .unwrap_or_default();
        let run_delegation = if delegates.is_empty() {
            None
        } else {
            Some(Arc::new(self.clone()) as Arc<dyn RunDelegationService>)
        };
        let boundary = crate::agent_runner::run_agent_until_boundary(
            self.llm.clone(),
            crate::agent_runner::AgentExecution {
                agent_id,
                model_ref: &self.model_ref,
                delegates: &delegates,
                run_delegation,
                context: Some(context),
                scheduler: self.scheduler.clone(),
            },
            sandbox,
            request,
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
                continuation: serde_json::json!({
                    "kind": "local_run",
                    "agent_id": agent_id,
                    "child_run_id": child_run_id.0,
                }),
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
        let (agent_id, _) = delegate_args(arguments);
        // A child is an ordinary Run and may reach HITL. Until it settles, the
        // parent cannot promise terminal-only completion to the batch barrier.
        // Independently dispatched children regain parallelism at the RunService
        // scheduler rather than by making this false promise.
        let _ = agent_id;
        false
    }

    async fn start(
        &self,
        request: DelegationRequest,
    ) -> Result<DelegationStep, DelegationExecutionError> {
        let (agent_id, input) = delegate_args(&request.arguments);
        if !self.may_delegate_to(&request.origin, &agent_id) {
            return Err(DelegationExecutionError::new(format!(
                "delegate agent {agent_id:?} is not in this Agent's roster"
            )));
        }
        if let Some(remote) = self.delegates.remotes.get(&agent_id) {
            return remote
                .run(
                    &agent_id,
                    &request.child_run_id.0,
                    &input,
                    request.context.cancellation.as_ref(),
                )
                .await;
        }
        self.native_boundary(
            &agent_id,
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
        let agent_id = request
            .continuation
            .get("agent_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                DelegationExecutionError::new("delegation continuation is missing agent_id")
            })?;
        if let Some(remote) = self.delegates.remotes.get(agent_id) {
            let input = match request.result {
                ResumeResult::ToolResult(output) => output.content,
                ResumeResult::Input(text) => text,
                ResumeResult::Decision { allow, note } => {
                    note.unwrap_or_else(|| if allow { "allow" } else { "deny" }.to_string())
                }
            };
            return remote
                .run(
                    agent_id,
                    &request.child_run_id.0,
                    &input,
                    request.context.cancellation.as_ref(),
                )
                .await;
        }
        if !self.may_delegate_to(&request.origin, agent_id) {
            return Err(DelegationExecutionError::new(format!(
                "delegate agent {agent_id:?} is not in this Agent's roster"
            )));
        }
        self.native_boundary(
            agent_id,
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
        if let Some(remote) = self.delegates.remotes.get(&cancellation.target_agent_id) {
            return remote
                .cancel(
                    &cancellation.target_agent_id,
                    &cancellation.child_run_id,
                    cancellation.execution_reference.as_ref(),
                )
                .await;
        }

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

impl SharedHost {
    /// Fetch a remote delegate's discovery card (outbound discovery) as neutral JSON.
    /// Fails if the agent is not a registered remote. The wire card shape lives in the
    /// adapter behind the [`RemoteAgent`] interface; the host only echoes the value.
    pub async fn remote_agent_card(&self, agent_id: &str) -> Result<Value, HostError> {
        let remote = self.delegates.remote(agent_id).ok_or_else(|| {
            HostError::bad_request(format!("agent {agent_id:?} is not a remote agent"))
        })?;
        remote
            .card(agent_id)
            .await
            .map_err(|e| HostError::internal(e.to_string()))
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
    use awaken_runtime::memory::MemoryCommitCoordinator;
    use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse};
    use awaken_runtime_contract::runtime_context::RuntimeRunContext;

    struct AwaitPermission;

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
        let sandbox = Arc::new(
            provider
                .create_sandbox(&crate::provisioning::agent_run_sandbox_spec("parent"))
                .await
                .expect("parent sandbox"),
        );
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
            model_access: None,
        };
        let mut delegates = Delegates::new();
        delegates.add_local(HashSet::from(["researcher".to_string()]));
        let service = HostRunDelegationService::new(
            Arc::new(AwaitPermission),
            "stub".to_string(),
            provider,
            sandbox,
            true,
            delegates,
            Some(scheduler),
        );
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
