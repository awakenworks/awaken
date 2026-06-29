//! The async agent execution loop.
//!
//! `execute` resolves the activation, runs the model/tool phase loop, stages a
//! `ThreadCommit`, and commits durable truth. Live progress is emitted to the
//! stream sink as a best-effort plane; it is never the replay source (G1/G13).

use async_trait::async_trait;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{Id as RunId, Lifecycle};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::commit::staged::ThreadCommit;
use awaken_agent_contract::event::draft::Draft as EventDraft;
use awaken_agent_contract::event::kind::Kind as EventKind;
use awaken_agent_contract::fact::run::Fact as RunFact;
use awaken_agent_contract::stream::event::{Event as StreamEvent, Kind as StreamKind};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::{Error, Result, RunExecutor, RunOutcome};
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatContent, ChatMessage, ChatRequest, ChatRole, ToolCall, ToolSchema,
};
use awaken_runtime_contract::permission::{GateOutcome, PermissionContext};
use awaken_runtime_contract::resolved::{ResolvedSpec, ToolDescriptor};
use awaken_runtime_contract::resolver::{self, RunResolver};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::tool::ToolOutput;

use crate::runtime::Runtime;

/// Upper bound on model/tool steps for one run. A natural-end text turn ends the
/// loop earlier; the bound only guards against a non-terminating tool cycle.
const MAX_STEPS: usize = 16;

#[async_trait]
impl RunExecutor for Runtime {
    async fn execute(
        &self,
        activation: RunActivation,
        context: RuntimeRunContext,
    ) -> Result<RunOutcome> {
        // Track this run's cancellation token so live control can steer it, and
        // always deregister on the way out.
        let run_id = activation.run_id.clone();
        if let Some(token) = &context.cancellation {
            self.register_run(&run_id, token.clone());
        }
        let result = run_agent_loop(self, activation, context).await;
        self.deregister_run(&run_id);
        result
    }
}

pub(crate) async fn run_agent_loop(
    runtime: &Runtime,
    activation: RunActivation,
    context: RuntimeRunContext,
) -> Result<RunOutcome> {
    let resolved = runtime
        .resolve(&activation.snapshot)
        .map_err(map_resolver_error)?;

    let run_id = activation.run_id.clone();
    let thread_id = activation.thread_id.clone();

    // Cancellation observed before any model call: commit a terminal Cancelled
    // outcome instead of starting work.
    if context.is_cancelled() {
        return finish(
            &context,
            &thread_id,
            run_id,
            Lifecycle::Cancelled,
            Vec::new(),
        )
        .await;
    }

    emit(&context, &run_id, StreamKind::RunStarted).await;

    let llm = runtime.llm().ok_or_else(|| {
        Error::Execution("no model provider configured for this runtime".to_string())
    })?;

    // The transcript starts from the activation input; new assistant/tool
    // messages are also collected for the commit.
    let mut transcript: Vec<Message> = activation.input.clone();
    let mut new_messages: Vec<Message> = Vec::new();

    let mut lifecycle = Lifecycle::Completed;

    for step in 0..MAX_STEPS {
        if context.is_cancelled() {
            lifecycle = Lifecycle::Cancelled;
            break;
        }

        let request = build_chat_request(&resolved.spec, &transcript);
        let response = llm
            .infer(request)
            .await
            .map_err(|err| Error::Execution(err.to_string()))?;

        // A cancel may have landed while inference was in flight; observe it at
        // this step boundary and discard the model output.
        if context.is_cancelled() {
            lifecycle = Lifecycle::Cancelled;
            break;
        }

        match response.output {
            AssistantOutput::Text(text) => {
                emit(
                    &context,
                    &run_id,
                    StreamKind::OutputText { text: text.clone() },
                )
                .await;
                let message = assistant_message(&run_id, step, text);
                transcript.push(message.clone());
                new_messages.push(message);
                break;
            }
            AssistantOutput::ToolCalls(calls) => {
                // Record the assistant's tool-call turn, then run each call
                // through the gate before execution (visibility != grant, G9/G21).
                let assistant = assistant_tool_call_message(&run_id, step, &calls);
                transcript.push(assistant.clone());
                new_messages.push(assistant);

                let mut suspended = false;
                for call in calls {
                    let output = match gate_decision(runtime, &call).await {
                        GateOutcome::Allow => execute_tool(runtime, &call).await,
                        GateOutcome::Block { reason } => {
                            ToolOutput::error(&call.call_id, format!("blocked: {reason}"))
                        }
                        GateOutcome::SetResult(output) => output,
                        GateOutcome::Suspend { .. } => {
                            suspended = true;
                            break;
                        }
                    };
                    let message = tool_result_message(&call, &output);
                    transcript.push(message.clone());
                    new_messages.push(message);
                }

                if suspended {
                    lifecycle = Lifecycle::Waiting;
                    break;
                }
                // Otherwise continue to the next model step with the results.
            }
        }
    }

    finish(&context, &thread_id, run_id, lifecycle, new_messages).await
}

/// Consult the permission gate; an absent gate allows (used only in tests).
async fn gate_decision(runtime: &Runtime, call: &ToolCall) -> GateOutcome {
    match runtime.gate() {
        Some(gate) => {
            let ctx = PermissionContext {
                tool_id: call.tool_id.clone(),
                call_id: call.call_id.clone(),
                arguments: call.arguments.clone(),
            };
            gate.gate(&ctx).await
        }
        None => GateOutcome::Allow,
    }
}

/// Invoke an authorized tool, turning a missing tool or a tool error into a
/// model-visible error result rather than aborting the run.
async fn execute_tool(runtime: &Runtime, call: &ToolCall) -> ToolOutput {
    match runtime.tool(&call.tool_id) {
        Some(tool) => match tool.invoke(call.clone()).await {
            Ok(output) => output,
            Err(err) => ToolOutput::error(&call.call_id, err.to_string()),
        },
        None => ToolOutput::error(&call.call_id, format!("unknown tool: {}", call.tool_id)),
    }
}

/// Build a model request from the resolved binding, transcript, and visible
/// tool descriptors.
pub(crate) fn build_chat_request(spec: &ResolvedSpec, transcript: &[Message]) -> ChatRequest {
    ChatRequest {
        model_binding: spec.model_binding.clone(),
        messages: transcript.iter().map(to_chat_message).collect(),
        tools: spec.tool_descriptors.iter().map(to_tool_schema).collect(),
    }
}

fn to_chat_message(message: &Message) -> ChatMessage {
    ChatMessage {
        role: to_chat_role(&message.role),
        content: ChatContent::Text(message.content.clone()),
    }
}

fn to_chat_role(role: &Role) -> ChatRole {
    match role {
        Role::System => ChatRole::System,
        Role::User => ChatRole::User,
        Role::Assistant => ChatRole::Assistant,
        Role::Tool => ChatRole::Tool,
    }
}

/// Project a pinned descriptor into the model-visible schema. The real
/// description and JSON Schema travel to the model, so it can call tools with
/// arguments; the descriptor's `content_hash` stays internal (G3/G8).
fn to_tool_schema(descriptor: &ToolDescriptor) -> ToolSchema {
    ToolSchema {
        id: descriptor.id.clone(),
        description: descriptor.description.clone(),
        parameters: descriptor.parameters.clone(),
    }
}

fn assistant_message(run_id: &RunId, step: usize, text: String) -> Message {
    Message {
        id: MessageId(format!("{}-assistant-{step}", run_id.0)),
        role: Role::Assistant,
        content: text,
    }
}

/// Record an assistant tool-call turn so the committed transcript explains why
/// tool results follow.
fn assistant_tool_call_message(run_id: &RunId, step: usize, calls: &[ToolCall]) -> Message {
    let summary: Vec<_> = calls.iter().map(|c| c.tool_id.as_str()).collect();
    Message {
        id: MessageId(format!("{}-assistant-{step}", run_id.0)),
        role: Role::Assistant,
        content: format!("tool_calls: {}", summary.join(", ")),
    }
}

fn tool_result_message(call: &ToolCall, output: &ToolOutput) -> Message {
    Message {
        id: MessageId(format!("tool-{}", call.call_id)),
        role: Role::Tool,
        content: output.content.clone(),
    }
}

/// Best-effort live emission. A sink failure is swallowed: committed truth is
/// authoritative, not the live stream (G10/G13).
async fn emit(context: &RuntimeRunContext, run_id: &RunId, kind: StreamKind) {
    if let Some(sink) = &context.stream_sink {
        let _ = sink
            .send(StreamEvent {
                run_id: run_id.clone(),
                kind,
            })
            .await;
    }
}

/// Stage and commit the terminal checkpoint, emit `RunFinished`, and return the
/// outcome. Commit only runs when a coordinator is wired and persistence is on.
async fn finish(
    context: &RuntimeRunContext,
    thread_id: &ThreadId,
    run_id: RunId,
    lifecycle: Lifecycle,
    new_messages: Vec<Message>,
) -> Result<RunOutcome> {
    if let Some(coordinator) = &context.commit {
        let commit = ThreadCommit {
            thread_id: thread_id.clone(),
            run_fact: RunFact {
                run_id: run_id.clone(),
                lifecycle: lifecycle.clone(),
            },
            messages: new_messages,
            state: Vec::new(),
            events: vec![EventDraft {
                kind: EventKind::RunLifecycleChanged,
                payload: serde_json::json!({ "lifecycle": lifecycle }),
            }],
        };
        coordinator
            .commit(commit)
            .await
            .map_err(|err| Error::Commit(err.to_string()))?;
    }

    emit(context, &run_id, StreamKind::RunFinished).await;
    Ok(RunOutcome { run_id, lifecycle })
}

fn map_resolver_error(err: resolver::Error) -> Error {
    Error::Resolution(err.to_string())
}
