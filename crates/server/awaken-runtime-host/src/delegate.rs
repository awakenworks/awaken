//! The host Run-delegation service: run a child Agent behind the `agent_run` tool.
//!
//! This is the process-startup implementation of [`RunDelegationService`].
//! The kernel routes the delegation tool to it; here, native (in-process Agent Run)
//! and remote agents are peer backends selected only from the delegated Agent's
//! immutable publication. This module owns child-Run admission; ordinary attempt
//! routing owns the Native/ACP/A2A execution edge.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_ext_builtin_tools::AGENT_RUN;
use awaken_runtime_contract::delegation::{
    ChildRunCancellation, DelegationExecutionError, DelegationRequest, DelegationResume,
    DelegationStep, DelegationToolInput, RunDelegationService,
};
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::resolved::{
    ADVISOR_TOOL_ID, ADVISOR_UNAVAILABLE_NOTICE, CatalogFingerprint,
};
use awaken_runtime_contract::resolver::PublishedAgentSnapshotSource;
use awaken_runtime_contract::snapshot::{AgentId, ExecutableAgentSnapshot};
#[cfg(test)]
use awaken_sandbox_local::LocalProvider;

use crate::agent_runner::{AgentRunBoundary, AgentRunSandbox, ChildRunRequest, RunScheduler};
use serde_json::Value;

/// Process-internal target identity for the publication-pinned advisor. It is
/// never a published delegate and cannot be selected through `agent_run`.
const INTERNAL_ADVISOR_AGENT_ID: &str = "__awaken_advisor";
const ADVISOR_REQUEST: &str = "Provide your advice for the primary agent now.";
const ADVISOR_INSTRUCTIONS: &str = "You are an advisor. Review the conversation and provide a concise, independent second opinion to the primary agent.";

#[derive(serde::Serialize)]
struct NativeDelegationContinuation {
    kind: &'static str,
    child_run_id: awaken_agent_contract::agent::run::Id,
}

fn child_seed_message(
    child_run_id: &awaken_agent_contract::agent::run::Id,
    input: String,
) -> Message {
    Message::text(
        MessageId(format!("{}-input", child_run_id.0)),
        Role::User,
        input,
    )
}

fn advisor_seed_messages(
    mut transcript: Vec<Message>,
    child_run_id: &awaken_agent_contract::agent::run::Id,
    parent_call_id: &str,
) -> Result<Vec<Message>, DelegationExecutionError> {
    let Some(current_step) = transcript.iter().rposition(|message| {
        message.role == Role::Assistant
            && message.content.iter().any(
                |block| matches!(block, ContentBlock::ToolUse { id, .. } if id == parent_call_id),
            )
    }) else {
        return Err(DelegationExecutionError::new(
            "advisor invocation is absent from the committed parent transcript",
        ));
    };
    if current_step + 1 != transcript.len() {
        return Err(DelegationExecutionError::new(
            "advisor invocation is not the current unresolved assistant Step",
        ));
    }

    // Every ToolUse in this assistant Step is still unresolved at the child-start
    // boundary. Copy the durable Step verbatim except for those unpaired blocks;
    // unlike the retired direct adapter, retain any text/thinking/media that the
    // primary emitted beside the calls.
    transcript[current_step]
        .content
        .retain(|block| !matches!(block, ContentBlock::ToolUse { .. }));
    if transcript[current_step].content.is_empty() {
        transcript.remove(current_step);
    }
    transcript.push(Message::text(
        MessageId(format!("{}-advisor-input", child_run_id.0)),
        Role::User,
        ADVISOR_REQUEST,
    ));
    Ok(transcript)
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
    /// Derived, tool-free child snapshot for the root Agent's pinned advisor.
    /// It is deliberately outside `targets`: advisor is not a published Agent
    /// edge and must never enter ordinary delegate lookup or recovery.
    advisor: Option<ExecutableAgentSnapshot>,
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
    fn target_is_terminal_without_external_action(snapshot: &ExecutableAgentSnapshot) -> bool {
        use awaken_runtime_contract::agent_bindings::ToolPermissionRequirement;
        use awaken_runtime_contract::resolved::ToolKind;

        let bindings = &snapshot.resolved_spec.plugin_config.agent;
        if bindings.toolsets.is_empty()
            || !bindings.mcp_servers.is_empty()
            || bindings.advisor.is_some()
            || !bindings.delegates.is_empty()
            || snapshot
                .resolved_spec
                .plugin_ids
                .iter()
                .any(|id| id != awaken_ext_builtin_tools::WEB_SEARCH_PLUGIN_ID)
        {
            return false;
        }
        snapshot
            .resolved_spec
            .tool_descriptors
            .iter()
            .filter(|tool| {
                bindings
                    .tool_policy(&tool.id)
                    .is_none_or(|policy| policy.enabled)
            })
            .all(|tool| {
                tool.kind == ToolKind::Regular
                    && bindings.tool_policy(&tool.id).is_some_and(|policy| {
                        policy.permission == ToolPermissionRequirement::AlwaysAllow
                    })
            })
    }

    pub(crate) fn new(
        llm: Arc<dyn LlmExecutor>,
        sandbox: Arc<crate::session_environment::SessionEnvironment>,
        parent_snapshot: &ExecutableAgentSnapshot,
        adapters: crate::agent_runner::ChildExecutionAdapters,
        publications: Option<Arc<dyn PublishedAgentSnapshotSource>>,
        workspace: String,
    ) -> Result<Self, DelegationExecutionError> {
        let targets = Self::resolve_targets(parent_snapshot, publications.as_deref(), &workspace)?;
        let advisor = Self::derive_advisor_snapshot(parent_snapshot)?;
        Ok(Self {
            llm,
            sandbox,
            targets,
            advisor,
            adapters,
            publications,
            workspace,
            scheduler: None,
        })
    }

    fn derive_advisor_snapshot(
        parent: &ExecutableAgentSnapshot,
    ) -> Result<Option<ExecutableAgentSnapshot>, DelegationExecutionError> {
        let Some(binding) = parent.resolved_spec.plugin_config.agent.advisor.as_ref() else {
            return Ok(None);
        };
        let instructions = if parent.resolved_spec.instructions.is_empty() {
            ADVISOR_INSTRUCTIONS.to_string()
        } else {
            format!(
                "{ADVISOR_INSTRUCTIONS} The primary agent's instructions are:\n{}",
                parent.resolved_spec.instructions
            )
        };
        let mut snapshot = ExecutableAgentSnapshot::builder(INTERNAL_ADVISOR_AGENT_ID)
            .instructions(instructions)
            .resolved_model(binding.candidate.clone())
            .inference_options(parent.resolved_spec.plugin_config.inference.clone())
            .max_steps(1)
            .build();
        snapshot.recompute_fingerprint().map_err(|error| {
            DelegationExecutionError::new(format!(
                "cannot fingerprint the derived advisor snapshot: {error}"
            ))
        })?;
        Ok(Some(snapshot))
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
            if binding.agent_id.0 == INTERNAL_ADVISOR_AGENT_ID {
                return Err(DelegationExecutionError::new(format!(
                    "delegate agent {:?} uses the reserved advisor identity",
                    binding.agent_id.0
                )));
            }
            if targets.contains_key(&binding.agent_id) {
                return Err(DelegationExecutionError::new(format!(
                    "delegate agent {:?} occurs more than once in the published targets",
                    binding.agent_id.0
                )));
            }
            let snapshot = awaken_runtime_contract::resolve_delegate_snapshot(
                parent,
                binding,
                publications,
                workspace,
            )
            .map_err(|_| {
                if binding.recursive_self {
                    DelegationExecutionError::new(
                        "recursive-self delegation target does not match its owner",
                    )
                } else if publications.is_none() {
                    DelegationExecutionError::new(format!(
                        "delegate agent {:?} has no publication source",
                        binding.agent_id.0
                    ))
                } else {
                    DelegationExecutionError::new(format!(
                        "delegate agent {:?} revision {:?} has no published executable snapshot",
                        binding.agent_id.0, binding.source_revision
                    ))
                }
            })?;
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
            // Advisor is primary-only. In particular, cloning the service for an
            // ordinary recursive delegate must not leak its parent's advisor
            // candidate into that child.
            child_service.advisor = None;
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

    async fn start_advisor(
        &self,
        request: DelegationRequest,
    ) -> Result<DelegationStep, DelegationExecutionError> {
        let snapshot = self
            .advisor
            .clone()
            .ok_or_else(|| DelegationExecutionError::new(ADVISOR_UNAVAILABLE_NOTICE))?;
        let transcript = request
            .context
            .reader
            .as_ref()
            .ok_or_else(|| {
                DelegationExecutionError::new(
                    "advisor consultation requires committed parent transcript access",
                )
            })?
            .committed_messages(&request.parent_thread_id);
        let seed = advisor_seed_messages(
            transcript,
            &request.child_run_id,
            &request.origin.parent_call_id,
        )?;
        let boundary = self
            .native_boundary(
                snapshot,
                ChildRunRequest {
                    seed: Some(seed.into()),
                    run_id: request.child_run_id,
                    origin: request.origin,
                    resume: None,
                    parent_thread_id: request.parent_thread_id,
                },
                request.context,
            )
            .await?;
        Ok(match boundary {
            // Advisor is a first-class coordinated Session Thread. Its committed
            // usage is folded exactly once by SessionApplication; returning it as
            // a parent rollup would duplicate that same child accounting fact.
            DelegationStep::Ended { text, .. } => DelegationStep::Ended {
                text,
                usage: Default::default(),
            },
            awaiting @ DelegationStep::Awaiting { .. } => awaiting,
        })
    }
}

#[async_trait]
impl RunDelegationService for HostRunDelegationService {
    fn tool_id(&self) -> &str {
        AGENT_RUN
    }

    fn handles_tool(&self, tool_id: &str) -> bool {
        tool_id == AGENT_RUN || (tool_id == ADVISOR_TOOL_ID && self.advisor.is_some())
    }

    fn supports_parallel_completion(&self, arguments: &Value) -> bool {
        let Ok(input) = DelegationToolInput::try_from(arguments) else {
            return false;
        };
        self.targets.get(&input.agent_id).is_some_and(|target| {
            Self::target_is_terminal_without_external_action(&target.snapshot)
        })
    }

    fn supports_parallel_completion_for(&self, tool_id: &str, arguments: &Value) -> bool {
        match tool_id {
            ADVISOR_TOOL_ID => self.advisor.is_some(),
            AGENT_RUN => self.supports_parallel_completion(arguments),
            _ => false,
        }
    }

    fn allows_recursive_target(&self, agent_id: &AgentId) -> bool {
        self.targets
            .get(agent_id)
            .is_some_and(|target| target.recursive_self)
    }

    fn target_agent_id(&self, arguments: &Value) -> Result<AgentId, DelegationExecutionError> {
        let target = DelegationToolInput::try_from(arguments)?.agent_id;
        if target.0 == INTERNAL_ADVISOR_AGENT_ID {
            return Err(DelegationExecutionError::new(
                "the reserved advisor target cannot be selected through agent_run",
            ));
        }
        Ok(target)
    }

    fn target_agent_id_for(
        &self,
        tool_id: &str,
        arguments: &Value,
    ) -> Result<AgentId, DelegationExecutionError> {
        match tool_id {
            ADVISOR_TOOL_ID if self.advisor.is_some() => {
                Ok(AgentId(INTERNAL_ADVISOR_AGENT_ID.to_string()))
            }
            ADVISOR_TOOL_ID => Err(DelegationExecutionError::new(ADVISOR_UNAVAILABLE_NOTICE)),
            AGENT_RUN => self.target_agent_id(arguments),
            other => Err(DelegationExecutionError::new(format!(
                "delegation service does not handle tool {other:?}"
            ))),
        }
    }

    async fn start(
        &self,
        request: DelegationRequest,
    ) -> Result<DelegationStep, DelegationExecutionError> {
        if request.target_agent_id.0 == INTERNAL_ADVISOR_AGENT_ID {
            return self.start_advisor(request).await;
        }
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
                // The child Run id is the durable idempotency identity, so its
                // synthesized seed message must also be stable. A generic
                // String -> RunInput conversion mints a process-local message
                // id and makes the same child dispatch differ after restart.
                seed: Some(child_seed_message(&request.child_run_id, input).into()),
                run_id: request.child_run_id,
                origin: request.origin,
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
        if agent_id.0 == INTERNAL_ADVISOR_AGENT_ID {
            return Err(DelegationExecutionError::new(
                "advisor child Runs are terminal-only and cannot resume",
            ));
        }
        if !self.targets.contains_key(&agent_id) {
            return Err(DelegationExecutionError::new(format!(
                "delegate agent {:?} is not in this Agent's published targets",
                agent_id.0
            )));
        }
        // The parent attempt reader is a single-claim projection and therefore
        // cannot own a local durable child's Thread. Reuse the scheduler's
        // Session-wide committed reader locally; a database-independent Worker
        // retains its existing claim-scoped path and never substitutes the parent
        // projection for a child projection.
        let ticket = self
            .scheduler
            .as_ref()
            .and_then(RunScheduler::local_child_reader)
            .or_else(|| request.context.reader.clone())
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
    use awaken_agent_contract::thread::commit::coordinator::Coordinator as _;
    use awaken_agent_contract::thread::commit::staged::{RunDisposition, ThreadCommit};
    use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
    use awaken_run_ingress::{AnyDispatchStore, DispatchQueue};
    use awaken_runtime_contract::agent_bindings::{
        AgentAdvisorBinding, AgentBindings, AgentDelegateBinding, InferenceOptions, ReasoningEffort,
    };
    use awaken_runtime_contract::llm::{
        AssistantOutput, ChatRequest, ChatResponse, ThreadUsage, TokenUsage,
    };
    use awaken_runtime_contract::resolved::{ModelBinding, ResolvedModelCandidate, ToolDescriptor};
    use awaken_runtime_contract::runtime_context::RuntimeRunContext;
    use awaken_store_inmem::MemoryCommitCoordinator;
    use std::collections::HashSet;
    use std::sync::Mutex;

    #[test]
    fn delegated_child_seed_identity_is_stable_across_process_reconstruction() {
        let child = RunId("stable-child".into());
        let first = child_seed_message(&child, "research".into());
        let reconstructed = child_seed_message(&child, "research".into());
        assert_eq!(first, reconstructed);
        assert_ne!(
            first.id,
            child_seed_message(&RunId("other-child".into()), "research".into()).id
        );
    }

    #[test]
    fn advisor_seed_is_a_stable_projection_of_the_committed_pre_call_step() {
        // Constraints/invariants: K1=the child Thread's committed state is the
        // sole Advisor-usage attribution; K2=SessionApplication's coordinated-
        // Thread fold is the sole cross-Thread aggregate; K3=native_boundary's
        // ordinary agent_run rollup remains unchanged.
        // Cause/effect graph: C1=current assistant Step contains the advisor call
        // beside stable text/media and another still-unresolved call; C2=the Step
        // contains only ToolUse; C3=the named call is absent or historical.
        // Effects: E1=copy every prior message/id and non-ToolUse block, strip all
        // unpaired calls from the current Step, append one deterministic explicit
        // request; E2=drop an emptied Step; E3=fail closed instead of consulting
        // against a guessed parent view.
        //
        // | Rule | current call | stable sibling content | effect            |
        // | S1   | latest       | present                | E1                |
        // | S2   | latest       | absent                 | E2 + stable input |
        // | S3   | absent/old   | either                 | E3                |
        let child = RunId("advisor-child".into());
        let prior = Message::text(MessageId("prior".into()), Role::User, "question");
        let current = Message::new(
            MessageId("current".into()),
            Role::Assistant,
            vec![
                ContentBlock::text("I will get a second opinion."),
                ContentBlock::tool_use("advisor-call", ADVISOR_TOOL_ID, serde_json::json!({})),
                ContentBlock::tool_use("sibling-call", "other", serde_json::json!({})),
            ],
        );
        let first =
            advisor_seed_messages(vec![prior.clone(), current.clone()], &child, "advisor-call")
                .expect("S1");
        let replay = advisor_seed_messages(vec![prior.clone(), current], &child, "advisor-call")
            .expect("S1 replay");
        assert_eq!(first, replay, "S1/E1 deterministic retry");
        assert_eq!(first[0], prior, "S1/E1 preserves prior identity/content");
        assert_eq!(first[1].id, MessageId("current".into()), "S1/E1");
        assert_eq!(first[1].text_content(), "I will get a second opinion.");
        assert!(
            first[1]
                .content
                .iter()
                .all(|block| !matches!(block, ContentBlock::ToolUse { .. })),
            "S1/E1"
        );
        assert_eq!(
            first.last(),
            Some(&Message::text(
                MessageId("advisor-child-advisor-input".into()),
                Role::User,
                ADVISOR_REQUEST,
            )),
            "S1/E1"
        );

        let tool_only = Message::new(
            MessageId("tool-only".into()),
            Role::Assistant,
            vec![ContentBlock::tool_use(
                "advisor-call",
                ADVISOR_TOOL_ID,
                serde_json::json!({}),
            )],
        );
        let projected =
            advisor_seed_messages(vec![prior.clone(), tool_only], &child, "advisor-call")
                .expect("S2");
        assert_eq!(projected.len(), 2, "S2/E2");
        assert_eq!(projected[0], prior, "S2/E2");

        assert!(
            advisor_seed_messages(Vec::new(), &child, "advisor-call").is_err(),
            "S3/E3 absent"
        );
        assert!(
            advisor_seed_messages(
                vec![
                    Message::new(
                        MessageId("old".into()),
                        Role::Assistant,
                        vec![ContentBlock::tool_use(
                            "advisor-call",
                            ADVISOR_TOOL_ID,
                            serde_json::json!({}),
                        )],
                    ),
                    Message::text(MessageId("later".into()), Role::User, "later"),
                ],
                &child,
                "advisor-call",
            )
            .is_err(),
            "S3/E3 historical"
        );
    }

    #[test]
    fn advisor_snapshot_pins_only_the_advisor_model_and_one_inference_step() {
        // Decision rule: execute every reachable cause partition documented here and
        // require its stated effects, including each fail-closed outcome.
        // Cause/effect graph: C1=parent has a pinned advisor candidate plus its
        // own tools/plugins/delegates/advisor/instructions/inference controls;
        // C2=no advisor binding. C1 -> E1 derive a content-addressed internal
        // snapshot with the exact candidate and inference controls, max_steps=1,
        // and every executable extension/child capability cleared; C2 -> E2 no
        // advisor target. The parent snapshot remains immutable in both rules.
        // Constraints/invariants: derivation may copy only the pinned advisor
        // candidate and inference controls; the immutable parent remains the sole
        // source and no parent tool, plugin, delegate, or advisor can leak through.
        //
        // | Rule | advisor | parent capabilities | effect                  |
        // | P1   | pinned  | populated           | E1 isolated child spec  |
        // | P2   | absent  | any                 | E2 no derived snapshot  |
        let candidate = ResolvedModelCandidate::host(ModelBinding::new(
            "advisor-provider",
            "advisor-model",
            "genai",
        ));
        let inference = InferenceOptions {
            effort: Some(ReasoningEffort::High),
            ..Default::default()
        };
        let parent = ExecutableAgentSnapshot::builder("coordinator")
            .model(ModelBinding::new(
                "parent-provider",
                "parent-model",
                "native",
            ))
            .instructions("Protect the user's constraints.")
            .tool(ToolDescriptor::pinned(
                "test",
                "parent-tool",
                "parent only",
                serde_json::json!({"type": "object"}),
            ))
            .plugins(["parent-plugin".to_string()])
            .plugin_config([("parent-plugin".to_string(), serde_json::json!({"x": 1}))])
            .agent_bindings(AgentBindings {
                delegates: vec![AgentDelegateBinding {
                    agent_id: AgentId("worker".into()),
                    source_revision: Some(7),
                    recursive_self: false,
                }],
                advisor: Some(AgentAdvisorBinding {
                    model: "public-advisor".into(),
                    candidate: candidate.clone(),
                }),
                ..Default::default()
            })
            .inference_options(inference.clone())
            .build();
        let unchanged = parent.clone();
        let advisor = HostRunDelegationService::derive_advisor_snapshot(&parent)
            .expect("P1")
            .expect("P1 target");
        assert_eq!(parent, unchanged, "P1 parent remains immutable");
        assert_eq!(advisor.root_agent_id.0, INTERNAL_ADVISOR_AGENT_ID, "P1/E1");
        assert_eq!(advisor.resolved_spec.model_binding, candidate, "P1/E1");
        assert_eq!(advisor.resolved_spec.max_steps, 1, "P1/E1");
        assert_eq!(
            advisor.resolved_spec.plugin_config.inference, inference,
            "P1/E1"
        );
        assert!(advisor.resolved_spec.model_candidates.is_empty(), "P1/E1");
        assert!(advisor.resolved_spec.tool_descriptors.is_empty(), "P1/E1");
        assert!(advisor.resolved_spec.plugin_ids.is_empty(), "P1/E1");
        assert!(
            advisor.resolved_spec.plugin_config.plugins().is_empty(),
            "P1/E1"
        );
        let bindings = &advisor.resolved_spec.plugin_config.agent;
        assert!(bindings.mcp_servers.is_empty(), "P1/E1");
        assert!(bindings.skills.is_empty(), "P1/E1");
        assert!(bindings.delegates.is_empty(), "P1/E1");
        assert!(bindings.advisor.is_none(), "P1/E1");
        assert!(bindings.toolsets.is_empty(), "P1/E1");
        assert!(
            advisor
                .resolved_spec
                .instructions
                .contains("Protect the user's constraints."),
            "P1/E1"
        );
        assert_ne!(advisor.fingerprint.0, INTERNAL_ADVISOR_AGENT_ID, "P1/E1");

        let without_advisor = ExecutableAgentSnapshot::builder("plain")
            .model(ModelBinding::new("plain-provider", "plain-model", "native"))
            .build();
        assert!(
            HostRunDelegationService::derive_advisor_snapshot(&without_advisor)
                .expect("P2")
                .is_none(),
            "P2/E2"
        );
    }

    struct AwaitPermission;

    #[derive(Default)]
    struct CaptureAdvisorRequests(Mutex<Vec<ChatRequest>>);

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
    fn only_children_proven_not_to_await_enter_the_parallel_barrier() {
        // Constraint/Invariant: the authoritative inputs and ownership boundaries
        // documented here remain the only decision source; no parallel path is admitted.
        // Decision rule: execute every reachable cause partition documented here and
        // require its stated effects, including each fail-closed outcome.
        use awaken_runtime_contract::agent_bindings::{
            ToolExecutionPolicy, ToolPermissionRequirement, ToolPolicyOverride, ToolsetPolicy,
            ToolsetSource,
        };

        // Cause/effect graph: C1 every enabled child tool is server-executed and
        // always_allow; C2 any tool can ask/client-execute; C3 the child owns MCP,
        // nested delegation, advisor, or an unclassified plugin. C1 -> E1 admit
        // the runtime's durable parallel child barrier. C2|C3 -> E2 retain the
        // serial boundary that can represent one exact HITL continuation.
        //
        // | Rule | C1 terminal proof | C2 external action | C3 open extension | Effect |
        // | R1   | yes               | no                 | no                | E1 parallel |
        // | R2   | no                | yes                | no                | E2 serial   |
        // | R3   | no                | either             | yes               | E2 serial   |
        //
        // FMECA: promising terminal completion for an always_ask/MCP child can
        // collapse several independently Awaiting child Threads into one parent
        // ticket; rejecting an all-allow closed child serializes Anthropic-style
        // fan-out. The immutable child publication is the sole proof source.
        let mut child = crate::config::server_config(
            "researcher",
            "stub",
            &HashSet::new(),
            &HashSet::new(),
            &[],
            &Default::default(),
            &[],
            awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
        );
        let overrides = child
            .resolved_spec
            .tool_descriptors
            .iter()
            .map(|tool| {
                ToolPolicyOverride::new(
                    tool.id.clone(),
                    ToolExecutionPolicy {
                        enabled: true,
                        permission: ToolPermissionRequirement::AlwaysAllow,
                    },
                )
            })
            .collect();
        child.resolved_spec.plugin_config.agent.toolsets = vec![ToolsetPolicy {
            source: ToolsetSource::Agent,
            default: ToolExecutionPolicy::default(),
            overrides,
        }];
        assert!(
            HostRunDelegationService::target_is_terminal_without_external_action(&child),
            "R1/E1"
        );

        child.resolved_spec.plugin_config.agent.toolsets[0].overrides[0]
            .policy
            .permission = ToolPermissionRequirement::AlwaysAsk;
        assert!(
            !HostRunDelegationService::target_is_terminal_without_external_action(&child),
            "R2/E2"
        );

        child.resolved_spec.plugin_config.agent.toolsets[0].overrides[0]
            .policy
            .permission = ToolPermissionRequirement::AlwaysAllow;
        child
            .resolved_spec
            .plugin_ids
            .push("unclassified-plugin".into());
        assert!(
            !HostRunDelegationService::target_is_terminal_without_external_action(&child),
            "R3/E2"
        );
    }

    #[test]
    fn delegation_target_resolution_freezes_exact_or_current_once() {
        // Constraint/Invariant: the authoritative inputs and ownership boundaries
        // documented here remain the only decision source; no parallel path is admitted.
        // Decision rule: execute every reachable cause partition documented here and
        // require its stated effects, including each fail-closed outcome.
        // Cause graph: C1=edge has an exact revision; C2=that publication exists;
        // C3=edge intentionally omits a revision. C1+C2 -> E1 freeze exact even
        // when current is newer; C1+!C2 -> E2 reject setup; !C1+C3 -> E3 resolve
        // current once and freeze it in the target map; C4=duplicate target ->
        // E4 reject rather than silently selecting one edge; C5=a published
        // delegate claims the internal advisor id -> E5 reject before lookup.
        //
        // | Rule | revision | publication | effect |
        // | V1 | 1 | v1 exists, current=v2 | freeze v1 |
        // | V2 | 99 | absent | fail before child start |
        // | V3 | none | current=v2 | freeze v2 once |
        // | V4 | duplicate | either | fail before child start |
        // | V5 | reserved id | either | fail before publication lookup |
        let first = ExecutableAgentSnapshot::builder("worker")
            .model(awaken_runtime_contract::resolved::ModelBinding::new(
                "test", "model", "native",
            ))
            .instructions("version one")
            .fingerprint("worker-v1")
            .build();
        let current = ExecutableAgentSnapshot::builder("worker")
            .model(awaken_runtime_contract::resolved::ModelBinding::new(
                "test", "model", "native",
            ))
            .instructions("version two")
            .fingerprint("worker-v2")
            .build();
        let source = VersionedPublications {
            first: first.clone(),
            current: current.clone(),
        };
        let parent = |source_revision| {
            ExecutableAgentSnapshot::builder("coordinator")
                .model(awaken_runtime_contract::resolved::ModelBinding::new(
                    "test", "model", "native",
                ))
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

        let reserved = ExecutableAgentSnapshot::builder("coordinator")
            .model(ModelBinding::new(
                "parent-provider",
                "parent-model",
                "native",
            ))
            .agent_bindings(AgentBindings {
                delegates: vec![AgentDelegateBinding {
                    agent_id: AgentId(INTERNAL_ADVISOR_AGENT_ID.into()),
                    source_revision: None,
                    recursive_self: false,
                }],
                ..Default::default()
            })
            .build();
        let error =
            HostRunDelegationService::resolve_targets(&reserved, Some(&source), "workspace")
                .expect_err("V5 reserved target must fail");
        assert!(error.to_string().contains("reserved advisor"), "V5/E5");
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

    #[async_trait]
    impl LlmExecutor for CaptureAdvisorRequests {
        async fn infer(
            &self,
            request: ChatRequest,
        ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
            self.0.lock().expect("capture lock").push(request);
            Ok(ChatResponse {
                output: AssistantOutput::text("independent advice"),
                usage: Some(TokenUsage {
                    prompt_tokens: 7,
                    completion_tokens: 3,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                }),
                stop_reason: None,
            })
        }
    }

    async fn local_test_sandbox() -> (
        tempfile::TempDir,
        Arc<crate::session_environment::SessionEnvironment>,
    ) {
        let root = tempfile::tempdir().expect("sandbox root");
        let provider = Arc::new(LocalProvider::new(root.path()));
        let sandbox = Arc::new(crate::session_environment::SessionEnvironment::workdir(
            provider
                .create_sandbox(&crate::provisioning::agent_run_sandbox_spec("parent"))
                .await
                .expect("parent sandbox"),
        ));
        (root, sandbox)
    }

    #[tokio::test]
    async fn advisor_uses_the_same_durable_native_child_boundary_as_agent_run() {
        // Constraint/Invariant: the authoritative inputs and ownership boundaries
        // documented here remain the only decision source; no parallel path is admitted.
        // Decision rule: execute every reachable cause partition documented here and
        // require its stated effects, including each fail-closed outcome.
        // Cause/effect graph: C1=the root publication pins an advisor; C2=the
        // parent has durably committed the current mixed-content advisor Step;
        // C3=the same child identity is retried; C4=agent_run names the reserved
        // target; C5=committed-history access is absent. C1+C2 -> E1 route
        // `advisor` to the internal target and execute
        // one tool-free child Run from the committed projection; +C3 -> E2 read
        // its terminal child truth without another model call; C4 -> E3 reject
        // the collision while ordinary agent_run decoding remains unchanged;
        // C5 -> E4 fail before child inference or persistence; C6=the Advisor
        // child reports non-zero model usage. C1+C2+C6 -> E5 commit that usage
        // only on the child Thread and return an empty parent-rollup delta;
        // +C3 -> E6 preserve the same attribution without another model call.
        //
        // | Rule | tool      | parent Step | child state | usage  | effect            |
        // | A1   | advisor   | committed   | absent      | nonzero| E1+E5 one child   |
        // | A2   | advisor   | committed   | Ended       | stored | E2+E6 exact replay |
        // | A3   | agent_run | n/a         | n/a         | n/a    | E3 reserve target  |
        // | A4   | advisor   | no reader   | absent      | n/a    | E4 fail closed      |
        let (_root, sandbox) = local_test_sandbox().await;
        let llm = Arc::new(CaptureAdvisorRequests::default());
        let store = Arc::new(
            AnyDispatchStore::open_sqlite_in_memory().expect("durable advisor dispatch store"),
        );
        let commit = Arc::new(MemoryCommitCoordinator::new());
        let scheduler = RunScheduler {
            store: store.clone(),
            commit: commit.clone(),
            reader: commit.clone(),
            owner: "advisor-child-owner".to_string(),
            claimed_commit: None,
            recovery_projection: None,
            session_resources: None,
            publication_source: None,
        };
        let candidate = ResolvedModelCandidate::host(ModelBinding::new(
            "advisor-provider",
            "advisor-model",
            "genai",
        ));
        let parent_snapshot = ExecutableAgentSnapshot::builder("coordinator")
            .model(ModelBinding::new(
                "parent-provider",
                "parent-model",
                "native",
            ))
            .instructions("Keep the answer grounded.")
            .agent_bindings(AgentBindings {
                advisor: Some(AgentAdvisorBinding {
                    model: "public-advisor".into(),
                    candidate: candidate.clone(),
                }),
                ..Default::default()
            })
            .build();
        let plain_snapshot = ExecutableAgentSnapshot::builder("plain")
            .model(ModelBinding::new("plain-provider", "plain-model", "native"))
            .build();
        let plain_service = HostRunDelegationService::new(
            llm.clone(),
            sandbox.clone(),
            &plain_snapshot,
            crate::agent_runner::ChildExecutionAdapters::default(),
            None,
            "default".into(),
        )
        .expect("advisor-free service");
        assert!(
            !plain_service.handles_tool(ADVISOR_TOOL_ID),
            "an absent binding never claims the advisor tool"
        );
        let service = HostRunDelegationService::new(
            llm.clone(),
            sandbox,
            &parent_snapshot,
            crate::agent_runner::ChildExecutionAdapters::default(),
            None,
            "default".into(),
        )
        .expect("A1 service")
        .with_scheduler(Some(scheduler));
        assert!(service.handles_tool(AGENT_RUN), "ordinary owner unchanged");
        assert!(service.handles_tool(ADVISOR_TOOL_ID), "A1/E1");
        assert!(
            service.supports_parallel_completion_for(ADVISOR_TOOL_ID, &serde_json::json!({})),
            "A1/E1 tool-free one-step proof"
        );
        assert_eq!(
            service
                .target_agent_id_for(ADVISOR_TOOL_ID, &serde_json::json!({}))
                .expect("A1 target")
                .0,
            INTERNAL_ADVISOR_AGENT_ID,
            "A1/E1"
        );
        assert_eq!(
            service
                .target_agent_id_for(
                    AGENT_RUN,
                    &serde_json::json!({"agent_id": "ordinary", "input": "work"}),
                )
                .expect("ordinary target")
                .0,
            "ordinary",
            "ordinary agent_run decoder is unchanged"
        );
        assert!(
            service
                .target_agent_id_for(
                    AGENT_RUN,
                    &serde_json::json!({
                        "agent_id": INTERNAL_ADVISOR_AGENT_ID,
                        "input": "collision"
                    }),
                )
                .is_err(),
            "A3/E3"
        );

        let parent_run_id = RunId("parent-run".into());
        let parent_thread_id = ThreadId("parent-thread".into());
        let origin =
            DelegationOrigin::root_for_agent(parent_run_id.clone(), "advisor-call", "coordinator");
        let child_run_id = origin.child_run_id();
        let missing_reader = match service
            .start(DelegationRequest {
                child_run_id: child_run_id.clone(),
                parent_thread_id: parent_thread_id.clone(),
                context: RuntimeRunContext::new(),
                origin: origin.clone(),
                target_agent_id: AgentId(INTERNAL_ADVISOR_AGENT_ID.into()),
                arguments: serde_json::json!({}),
            })
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("A4 expected missing committed-history authority to fail"),
        };
        assert!(
            missing_reader
                .to_string()
                .contains("committed parent transcript"),
            "A4/E4"
        );
        assert!(llm.0.lock().expect("capture lock").is_empty(), "A4/E4");
        let parent_user = Message::text(MessageId("parent-user".into()), Role::User, "question");
        let current_assistant = Message::new(
            MessageId::assistant(&parent_run_id, 0),
            Role::Assistant,
            vec![
                ContentBlock::text("I will verify this."),
                ContentBlock::tool_use("advisor-call", ADVISOR_TOOL_ID, serde_json::json!({})),
            ],
        );
        commit
            .commit(ThreadCommit::assemble(
                parent_thread_id.clone(),
                RunDisposition::running(parent_run_id),
                true,
                vec![parent_user.clone(), current_assistant.clone()],
                Vec::new(),
                Vec::new(),
            ))
            .await
            .expect("commit parent transcript");
        let invoke = || DelegationRequest {
            child_run_id: child_run_id.clone(),
            parent_thread_id: parent_thread_id.clone(),
            context: RuntimeRunContext::new()
                .with_commit(commit.clone())
                .with_reader(commit.clone()),
            origin: origin.clone(),
            target_agent_id: AgentId(INTERNAL_ADVISOR_AGENT_ID.into()),
            arguments: serde_json::json!({}),
        };

        let first = service.start(invoke()).await.expect("A1");
        match first {
            DelegationStep::Ended { text, usage } => {
                assert_eq!(text, "independent advice", "A1/E1");
                assert!(usage.is_empty(), "A1/E5 parent rollup stays empty");
            }
            DelegationStep::Awaiting { .. } => panic!("A1 expected terminal advice"),
        }
        {
            let requests = llm.0.lock().expect("capture lock");
            assert_eq!(requests.len(), 1, "A1/E1");
            let request = &requests[0];
            assert_eq!(&request.model_binding, candidate.binding(), "A1/E1");
            assert!(request.tools.is_empty(), "A1/E1");
            assert!(
                request.messages.iter().all(|message| message
                    .content
                    .iter()
                    .all(|block| !matches!(block, ContentBlock::ToolUse { .. }))),
                "A1/E1 no unresolved provider transcript pair"
            );
            assert!(
                request.messages.iter().any(|message| {
                    message.role == Role::Assistant
                        && message.content == vec![ContentBlock::text("I will verify this.")]
                }),
                "A1/E1 stable sibling content"
            );
            assert!(
                request.messages.iter().any(|message| {
                    message.role == Role::User
                        && awaken_agent_contract::agent::content::extract_text(&message.content)
                            == ADVISOR_REQUEST
                }),
                "A1/E1 explicit consultation input"
            );
        }

        let child_thread = ThreadId(child_run_id.0.clone());
        let child_messages = commit.committed_messages(&child_thread);
        assert_eq!(child_messages[0], parent_user, "A1/E1 copied stable input");
        assert_eq!(child_messages[1].id, current_assistant.id, "A1/E1");
        assert_eq!(child_messages[1].text_content(), "I will verify this.");
        assert!(
            child_messages.iter().all(|message| message
                .content
                .iter()
                .all(|block| !matches!(block, ContentBlock::ToolUse { .. }))),
            "A1/E1"
        );
        assert_eq!(
            commit.run_state(&child_run_id),
            Some(RunState::Ended(EndCause::NaturalEnd)),
            "A1/E1"
        );
        let child_usage = ThreadUsage::from_committed_state(&commit.committed_state(&child_thread));
        assert_eq!(
            child_usage.total(),
            TokenUsage {
                prompt_tokens: 7,
                completion_tokens: 3,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            },
            "A1/E5 child Thread keeps the exact model usage"
        );

        let replay = service.start(invoke()).await.expect("A2");
        match replay {
            DelegationStep::Ended { text, usage } => {
                assert_eq!(text, "independent advice", "A2/E2");
                assert!(usage.is_empty(), "A2/E6 parent rollup stays empty");
            }
            DelegationStep::Awaiting { .. } => panic!("A2 expected terminal replay"),
        }
        assert_eq!(llm.0.lock().expect("capture lock").len(), 1, "A2/E2");
        assert_eq!(
            ThreadUsage::from_committed_state(&commit.committed_state(&child_thread)),
            child_usage,
            "A2/E6 replay preserves the exact child attribution"
        );
    }

    /// Cause/effect design: C1 a native child dispatch reaches Awaiting; C2 a
    /// replacement process submits its durable cancellation; C3 that cancellation
    /// is retried without an in-memory delivery receipt. Effects: E1 C2 commits
    /// Cancelled and removes the dispatch; E2 C3 is an idempotent no-op preserving
    /// the same terminal truth. Decision table: D1=C1+C2=>E1; D2=E1+C3=>E2.
    /// Constraint/Invariant: the durable child dispatch and committed Run are the
    /// only cancellation authorities. Decision rule: execute D1 then replay D2.
    #[tokio::test]
    async fn replacement_process_cancels_an_awaiting_native_child_idempotently() {
        // Decision rule: execute durable cancellation D1, then exact replay D2.
        let (_root, sandbox) = local_test_sandbox().await;
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
            publication_source: None,
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
