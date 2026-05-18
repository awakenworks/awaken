use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use async_trait::async_trait;
use awaken_contract::StateError;
use awaken_contract::contract::tool::ToolStatus;
use awaken_contract::identity::agent_prompt_id;
use awaken_contract::registry_spec::AgentSpec;
use awaken_runtime::extensions::background::{BackgroundTaskStateKey, PersistedTaskMeta};
use awaken_runtime::registry::RegistrySnapshot;
use awaken_runtime::{PhaseContext, PhaseHook, StateCommand};

use crate::metrics::{
    BackgroundTaskSpan, DelegationSpan, GenAISpan, HandoffSpan, MetricsEvent, SpanContext,
    SuspensionSpan, ToolSpan, is_tool_payload_truncated,
};

use awaken_contract::contract::inference::{StopReason, StreamResult};
use serde_json::Value;

use crate::metrics::ContentCapture;

use super::shared::{Inner, extract_cache_tokens, extract_token_counts};

/// Serialise the assistant turn's content blocks and tool calls into
/// JSON for `GenAISpan::response_content` / `response_tool_calls` when
/// [`ContentCapture`] is enabled. Empty vecs serialise to `None` so the
/// span stays compact for the common case of a chat-only or tool-only
/// turn. The `StreamResult` types implement `Serialize` infallibly, so a
/// serialisation failure here is a logic bug — fall back to `None` and
/// keep the rest of the span intact rather than aborting the run.
fn capture_response_payload(
    capture: ContentCapture,
    ok_result: Option<&StreamResult>,
) -> (Option<Value>, Option<Value>) {
    if !capture.is_enabled() {
        return (None, None);
    }
    let Some(result) = ok_result else {
        return (None, None);
    };
    let content = if result.content.is_empty() {
        None
    } else {
        serde_json::to_value(&result.content).ok()
    };
    let tool_calls = if result.tool_calls.is_empty() {
        None
    } else {
        serde_json::to_value(&result.tool_calls).ok()
    };
    (content, tool_calls)
}

/// Serialise the request message history into JSON for
/// `GenAISpan::request_messages`. Only fires for the first inference of
/// the run (step == 0) when [`ContentCapture`] is enabled — later spans
/// would carry growing copies of the same history, so paying that
/// `O(turns²)` storage cost has no upside for the trace→fixture flow.
fn capture_request_messages(
    capture: ContentCapture,
    step: u32,
    messages: &[std::sync::Arc<awaken_contract::contract::message::Message>],
) -> Option<Value> {
    if !capture.is_enabled() || step != 0 || messages.is_empty() {
        return None;
    }
    serde_json::to_value(messages).ok()
}

/// Prefix used by AgentTool descriptors (`agent_run_{agent_id}`).
const DELEGATION_TOOL_PREFIX: &str = "agent_run_";

// GenAI semantic-convention finish_reason wire strings, kept as named
// constants so the OTel-bound values live in one place.
const FINISH_REASON_END_TURN: &str = "end_turn";
const FINISH_REASON_MAX_TOKENS: &str = "max_tokens";
const FINISH_REASON_TOOL_USE: &str = "tool_use";
const FINISH_REASON_STOP_SEQUENCE: &str = "stop_sequence";

/// Map an upstream `StopReason` to its GenAI semantic-convention wire
/// representation. Single source of truth for the four finish-reason
/// strings emitted on inference spans.
fn stop_reason_to_finish_reason(reason: StopReason) -> &'static str {
    match reason {
        StopReason::EndTurn => FINISH_REASON_END_TURN,
        StopReason::MaxTokens => FINISH_REASON_MAX_TOKENS,
        StopReason::ToolUse => FINISH_REASON_TOOL_USE,
        StopReason::StopSequence => FINISH_REASON_STOP_SEQUENCE,
    }
}

fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Resolve `prompt_id` for the current turn. Prefers the resolved
/// `agent_spec`'s system prompt (the literal text the LLM will see this
/// turn) and falls back to the registry snapshot when the spec has not
/// been hydrated yet at this phase boundary. Used by both `RunStartHook`
/// and the handoff branch of `BeforeInferenceHook` so every stamp obeys
/// the same precedence and a handoff into an agent whose spec is still a
/// stub does not silently drop the attribution.
fn derive_prompt_id(agent_spec: &AgentSpec, snapshot: Option<&RegistrySnapshot>) -> Option<String> {
    if !agent_spec.system_prompt.is_empty() {
        Some(agent_prompt_id(
            &agent_spec.id,
            "system",
            &agent_spec.system_prompt,
        ))
    } else {
        snapshot.and_then(|s| s.agent_prompt_id(&agent_spec.id))
    }
}

pub(crate) struct RunStartHook(pub(crate) Arc<Inner>);

#[async_trait]
impl PhaseHook for RunStartHook {
    async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        *self.0.run_start.lock().await = Some(Instant::now());
        *self.0.metrics.lock().await = crate::metrics::AgentMetrics::default();
        // `background_task_statuses` is intentionally NOT cleared here. Its
        // role is to remember which (owner_thread_id, task_id, status)
        // tuples have already been emitted to the sink. Persisted background
        // task snapshots may survive across runs, so clearing on RunStart
        // would cause already-completed tasks to be re-emitted as new events.
        // The map is bounded by total background tasks ever observed, which
        // is small for any realistic workload.
        self.0.inference_tracing_span.lock().await.take();
        self.0.tool_tracing_span.lock().await.clear();
        self.0.tool_start.lock().await.clear();

        // Capture execution context from RunIdentity for all subsequent spans.
        let ri = &ctx.run_identity;
        // ADR-0030 D2: stamp content-addressed prompt_id via the shared
        // helper so RunStart and handoff use identical precedence.
        let prompt_id = derive_prompt_id(&ctx.agent_spec, ctx.registry_snapshot.as_deref());
        *self.0.span_context.lock().await = SpanContext {
            run_id: ri.run_id.clone(),
            thread_id: ri.thread_id.clone(),
            agent_id: ri.agent_id.clone(),
            parent_run_id: ri.parent_run_id.clone(),
            parent_tool_call_id: ri.parent_tool_call_id.clone(),
            prompt_id,
            // skill_ids stays empty — see RegistrySnapshot's in-code note
            // about skill_content_id being deferred to a follow-up ADR.
            skill_ids: Vec::new(),
            ..Default::default()
        };
        // Reset step counter for the new run.
        self.0.step_counter.store(0, Ordering::Relaxed);

        Ok(StateCommand::new())
    }
}

pub(crate) struct BeforeInferenceHook(pub(crate) Arc<Inner>);

#[async_trait]
impl PhaseHook for BeforeInferenceHook {
    async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        let s = &self.0;

        // Detect agent handoff: if the agent_id changed since last inference,
        // emit a HandoffSpan so Phoenix / external observers see the switch.
        {
            let current_ctx = s.span_context.lock().await;
            let new_agent_id = &ctx.run_identity.agent_id;
            if !current_ctx.agent_id.is_empty()
                && !new_agent_id.is_empty()
                && current_ctx.agent_id != *new_agent_id
            {
                let handoff = HandoffSpan {
                    context: current_ctx.clone(),
                    from_agent_id: current_ctx.agent_id.clone(),
                    to_agent_id: new_agent_id.clone(),
                    reason: None,
                    timestamp_ms: now_epoch_ms(),
                };
                // Must drop the lock before acquiring metrics lock.
                drop(current_ctx);
                crate::prometheus::record_handoff(&handoff);
                s.sink.record(&MetricsEvent::Handoff(handoff.clone()));
                s.metrics.lock().await.handoffs.push(handoff);
                // Update span context with the new agent's identity AND
                // its content-addressed prompt id. F13: previously only
                // `agent_id` was refreshed, so subsequent inference
                // spans carried the new agent_id but the old agent's
                // prompt_id — corrupting attribution after every
                // handoff. `ctx.agent_spec` here is the new agent's
                // spec (re-resolved at the handoff boundary, ADR-0014
                // D3), so we recompute prompt_id from it through the
                // shared `derive_prompt_id` helper. Going through the
                // helper also picks up the snapshot fallback for the
                // handoff path, matching RunStart semantics — without
                // it, a handoff into an agent whose spec still has an
                // empty `system_prompt` would silently null the
                // attribution.
                let mut sc = s.span_context.lock().await;
                sc.agent_id = new_agent_id.clone();
                sc.prompt_id = derive_prompt_id(&ctx.agent_spec, ctx.registry_snapshot.as_deref());
            }
        }

        // Close any abandoned inference tracing span from a retried attempt.
        if let Some(previous_span) = s.inference_tracing_span.lock().await.take() {
            let message = "A previous inference attempt was retried before completion.";
            previous_span.record("error.type", "inference_retry_interrupted");
            previous_span.record("error.message", message);
            previous_span.record("otel.status_code", "ERROR");
            previous_span.record("otel.status_description", message);
            drop(previous_span);
        }

        // ADR-0030 D2: populate tool_desc_ids from the registry snapshot.
        // allowed_tools on agent_spec lists the tool IDs advertised at this turn;
        // tool_desc_id computes the content-addressed hash of each descriptor.
        if let Some(snapshot) = ctx.registry_snapshot.as_deref() {
            let allowed: Vec<String> = ctx
                .agent_spec
                .allowed_tools
                .as_deref()
                .map(|ids| ids.to_vec())
                .unwrap_or_else(|| snapshot.registries().tools.tool_ids());
            let tool_desc_ids: Vec<String> = allowed
                .iter()
                .filter_map(|id| snapshot.tool_desc_id(id))
                .collect();
            let mut span_ctx = s.span_context.lock().await;
            span_ctx.tool_desc_ids = tool_desc_ids;
        }

        *s.inference_start.lock().await = Some((Instant::now(), now_epoch_ms()));

        let model = s.model.lock().await.clone();
        let provider = s.provider.lock().await.clone();
        let span_name = format!("{} {}", s.operation, model);
        let span = tracing::info_span!("gen_ai",
            "otel.name" = %span_name,
            "otel.kind" = "client",
            "otel.status_code" = tracing::field::Empty,
            "otel.status_description" = tracing::field::Empty,
            "gen_ai.provider.name" = %provider,
            "gen_ai.operation.name" = %s.operation,
            "gen_ai.request.model" = %model,
            "gen_ai.request.temperature" = tracing::field::Empty,
            "gen_ai.request.top_p" = tracing::field::Empty,
            "gen_ai.request.max_tokens" = tracing::field::Empty,
            "gen_ai.request.stop_sequences" = tracing::field::Empty,
            "gen_ai.response.model" = tracing::field::Empty,
            "gen_ai.response.id" = tracing::field::Empty,
            "gen_ai.usage.reasoning.output_tokens" = tracing::field::Empty,
            "gen_ai.usage.input_tokens" = tracing::field::Empty,
            "gen_ai.usage.output_tokens" = tracing::field::Empty,
            "gen_ai.response.finish_reasons" = tracing::field::Empty,
            "gen_ai.usage.cache_read.input_tokens" = tracing::field::Empty,
            "gen_ai.usage.cache_creation.input_tokens" = tracing::field::Empty,
            "error.type" = tracing::field::Empty,
            "error.message" = tracing::field::Empty,
            "gen_ai.error.class" = tracing::field::Empty,
        );

        if let Some(t) = *s.temperature.lock().await {
            span.record("gen_ai.request.temperature", t);
        }
        if let Some(t) = *s.top_p.lock().await {
            span.record("gen_ai.request.top_p", t);
        }
        if let Some(t) = *s.max_tokens.lock().await {
            span.record("gen_ai.request.max_tokens", t as i64);
        }
        {
            let seqs = s.stop_sequences.lock().await;
            if !seqs.is_empty() {
                span.record(
                    "gen_ai.request.stop_sequences",
                    format!("{:?}", *seqs).as_str(),
                );
            }
        }
        *s.inference_tracing_span.lock().await = Some(span);

        Ok(StateCommand::new())
    }
}

pub(crate) struct AfterInferenceHook(pub(crate) Arc<Inner>);

#[async_trait]
impl PhaseHook for AfterInferenceHook {
    async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        let s = &self.0;

        let (duration_ms, started_at_ms) = s
            .inference_start
            .lock()
            .await
            .take()
            .map(|(instant, started_at_ms)| (instant.elapsed().as_millis() as u64, started_at_ms))
            .unwrap_or((0, now_epoch_ms()));
        let ended_at_ms = started_at_ms.saturating_add(duration_ms);

        // Extract usage, error, and the success result from the LLM response.
        let (usage, error, ok_result) = match &ctx.llm_response {
            Some(resp) => match &resp.outcome {
                Ok(result) => (result.usage.as_ref(), None, Some(result)),
                Err(err) => (None, Some(err), None),
            },
            None => (None, None, None),
        };

        // Map the upstream stop_reason to the GenAI semantic-convention
        // finish_reasons list.
        let finish_reasons: Vec<String> = ok_result
            .and_then(|r| r.stop_reason)
            .map(|reason| vec![stop_reason_to_finish_reason(reason).to_string()])
            .unwrap_or_default();

        let (input_tokens, output_tokens, total_tokens, thinking_tokens) =
            extract_token_counts(usage);
        let (cache_read_input_tokens, cache_creation_input_tokens) = extract_cache_tokens(usage);

        let mut context = s.span_context.lock().await.clone();
        // F2: prefer the post-filter tool list threaded onto the
        // PhaseContext by the loop runner over the pre-filter
        // approximation BeforeInferenceHook computed from
        // `agent_spec.allowed_tools`. This stamps the GenAI span with the
        // tools the LLM actually saw, including any inclusion/exclusion
        // payloads from gate hooks and any frontend tools the
        // orchestrator merged in.
        if let Some(ids) = &ctx.effective_tool_ids {
            context.tool_desc_ids = ids.clone();
        }
        let step = s.step_counter.fetch_add(1, Ordering::Relaxed);
        let model = s.model.lock().await.clone();
        let provider = s.provider.lock().await.clone();
        let (response_content, response_tool_calls) =
            capture_response_payload(s.content_capture, ok_result);
        let request_messages = capture_request_messages(s.content_capture, step, &ctx.messages);
        let span = GenAISpan {
            context,
            step_index: Some(step),
            model,
            provider,
            operation: s.operation.clone(),
            // Not surfaced by `StreamResult` (see
            // `awaken_contract::contract::inference::StreamResult`); the
            // upstream provider's model id and response id are dropped on the
            // floor by `StreamCollector::finish`, so populating these requires
            // a contract extension. Tracked as the next step on ADR-0030 D8.
            response_model: None,
            response_id: None,
            finish_reasons,
            error_type: error.map(|e| e.error_type.clone()),
            error_class: error.and_then(|e| e.error_class.clone()),
            input_tokens,
            output_tokens,
            total_tokens,
            thinking_tokens,
            cache_read_input_tokens,
            cache_creation_input_tokens,
            temperature: *s.temperature.lock().await,
            top_p: *s.top_p.lock().await,
            max_tokens: *s.max_tokens.lock().await,
            stop_sequences: s.stop_sequences.lock().await.clone(),
            duration_ms,
            started_at_ms,
            ended_at_ms,
            response_content,
            response_tool_calls,
            request_messages,
        };

        // Record tracing span attributes.
        if let Some(tracing_span) = s.inference_tracing_span.lock().await.take() {
            if let Some(v) = span.thinking_tokens {
                tracing_span.record("gen_ai.usage.reasoning.output_tokens", v);
            }
            if let Some(v) = span.input_tokens {
                tracing_span.record("gen_ai.usage.input_tokens", v);
            }
            if let Some(v) = span.output_tokens {
                tracing_span.record("gen_ai.usage.output_tokens", v);
            }
            if let Some(v) = span.cache_read_input_tokens {
                tracing_span.record("gen_ai.usage.cache_read.input_tokens", v);
            }
            if let Some(v) = span.cache_creation_input_tokens {
                tracing_span.record("gen_ai.usage.cache_creation.input_tokens", v);
            }
            if !span.finish_reasons.is_empty() {
                tracing_span.record(
                    "gen_ai.response.finish_reasons",
                    format!("{:?}", span.finish_reasons).as_str(),
                );
            }
            if let Some(ref v) = span.response_model {
                tracing_span.record("gen_ai.response.model", v.as_str());
            }
            if let Some(ref v) = span.response_id {
                tracing_span.record("gen_ai.response.id", v.as_str());
            }
            if let Some(err) = error {
                tracing_span.record("error.type", err.error_type.as_str());
                tracing_span.record("error.message", err.message.as_str());
                tracing_span.record("otel.status_code", "ERROR");
                tracing_span.record("otel.status_description", err.message.as_str());
                if let Some(ref class) = err.error_class {
                    tracing_span.record("gen_ai.error.class", class.as_str());
                }
            }
            drop(tracing_span);
        }

        crate::prometheus::record_inference(&span);
        s.sink.record(&MetricsEvent::Inference(span.clone()));
        s.metrics.lock().await.inferences.push(span);

        Ok(StateCommand::new())
    }
}

pub(crate) struct BeforeToolExecuteHook(pub(crate) Arc<Inner>);

#[async_trait]
impl PhaseHook for BeforeToolExecuteHook {
    async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        let s = &self.0;

        let tool_name = ctx.tool_name.as_deref().unwrap_or_default().to_string();
        let call_id = ctx.tool_call_id.as_deref().unwrap_or_default().to_string();

        if !call_id.is_empty() {
            s.tool_start
                .lock()
                .await
                .insert(call_id.clone(), (Instant::now(), now_epoch_ms()));
        }

        // ADR-0030 D2: stamp the single tool_desc_id for this call into the
        // span context so AfterToolExecute captures it on the ToolSpan.
        if let Some(snapshot) = ctx.registry_snapshot.as_deref() {
            if let Some(desc_id) = snapshot.tool_desc_id(&tool_name) {
                s.span_context.lock().await.tool_desc_ids = vec![desc_id];
            } else {
                s.span_context.lock().await.tool_desc_ids = Vec::new();
            }
        }

        let provider = s.provider.lock().await.clone();
        let span_name = format!("execute_tool {}", tool_name);
        let span = tracing::info_span!("gen_ai",
            "otel.name" = %span_name,
            "otel.kind" = "internal",
            "otel.status_code" = tracing::field::Empty,
            "otel.status_description" = tracing::field::Empty,
            "gen_ai.provider.name" = %provider,
            "gen_ai.operation.name" = "execute_tool",
            "gen_ai.tool.name" = %tool_name,
            "gen_ai.tool.call.id" = %call_id,
            "gen_ai.tool.type" = "function",
            "gen_ai.tool.call.arguments" = tracing::field::Empty,
            "gen_ai.tool.call.result" = tracing::field::Empty,
            "awaken.tool.payload.truncated" = tracing::field::Empty,
            "error.type" = tracing::field::Empty,
            "error.message" = tracing::field::Empty,
        );

        if s.tool_io_capture.captures_arguments()
            && let Some(args) = &ctx.tool_args
        {
            let sanitized = s.sanitize_tool_payload(args);
            if is_tool_payload_truncated(&sanitized) {
                span.record("awaken.tool.payload.truncated", true);
            }
            if let Ok(serialized) = serde_json::to_string(&sanitized) {
                span.record("gen_ai.tool.call.arguments", serialized.as_str());
            }
        }

        if !call_id.is_empty() {
            s.tool_tracing_span
                .lock()
                .await
                .insert(call_id.clone(), span);
        }

        // Detect tool resume: if resume_input is present, this is a previously
        // suspended tool call being resumed.
        if let Some(resume) = &ctx.resume_input {
            let context = s.span_context.lock().await.clone();
            let resume_mode = match resume.action {
                awaken_contract::contract::suspension::ResumeDecisionAction::Resume => "resume",
                awaken_contract::contract::suspension::ResumeDecisionAction::Cancel => "cancel",
            };
            let suspension = SuspensionSpan {
                context,
                tool_call_id: call_id,
                tool_name,
                action: "resumed".to_string(),
                resume_mode: Some(resume_mode.to_string()),
                duration_ms: None,
                timestamp_ms: now_epoch_ms(),
            };
            crate::prometheus::record_suspension(&suspension);
            s.sink.record(&MetricsEvent::Suspension(suspension.clone()));
            s.metrics.lock().await.suspensions.push(suspension);
        }

        Ok(StateCommand::new())
    }
}

pub(crate) struct AfterToolExecuteHook(pub(crate) Arc<Inner>);

#[async_trait]
impl PhaseHook for AfterToolExecuteHook {
    async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        let s = &self.0;

        let call_id = ctx.tool_call_id.as_deref().unwrap_or_default().to_string();
        let (duration_ms, started_at_ms) = s
            .tool_start
            .lock()
            .await
            .remove(&call_id)
            .map(|(instant, started_at_ms)| (instant.elapsed().as_millis() as u64, started_at_ms))
            .unwrap_or((0, now_epoch_ms()));
        let ended_at_ms = started_at_ms.saturating_add(duration_ms);

        let tracing_span = s.tool_tracing_span.lock().await.remove(&call_id);

        // Treat a missing `tool_result` as a synthetic tool failure so the
        // sink, Prometheus counters, and any pending OTel tool context all
        // see one terminal event for this call. Without this fallback the
        // only signal would be a silent tracing-span drop, which masks the
        // failure from downstream consumers.
        let context = s.span_context.lock().await.clone();
        let step = s.step_counter.load(Ordering::Relaxed).saturating_sub(1);
        let captured_args = if s.tool_io_capture.captures_arguments() {
            ctx.tool_args
                .as_ref()
                .map(|value| s.sanitize_tool_payload(value))
        } else {
            None
        };
        let Some(result) = ctx.tool_result.as_ref() else {
            let tool_name = ctx.tool_name.clone().unwrap_or_default();
            let span = ToolSpan {
                context,
                step_index: Some(step),
                name: tool_name,
                operation: "execute_tool".to_string(),
                call_id: call_id.clone(),
                tool_type: "function".to_string(),
                call_arguments: captured_args,
                call_result: None,
                error_type: Some("missing_tool_result".to_string()),
                duration_ms,
                started_at_ms,
                ended_at_ms,
            };
            if let Some(tracing_span) = tracing_span {
                tracing_span.record("error.type", "missing_tool_result");
                tracing_span.record("otel.status_code", "ERROR");
                tracing_span.record(
                    "otel.status_description",
                    "AfterToolExecute fired without tool_result",
                );
                drop(tracing_span);
            }
            crate::prometheus::record_tool(&span);
            s.sink.record(&MetricsEvent::Tool(span.clone()));
            s.metrics.lock().await.tools.push(span);
            return Ok(StateCommand::new());
        };

        let error_type = if result.status == ToolStatus::Error {
            Some("tool_error".to_string())
        } else {
            None
        };
        let error_message = result.message.clone().filter(|_| error_type.is_some());

        let span = ToolSpan {
            context,
            step_index: Some(step),
            name: result.tool_name.clone(),
            operation: "execute_tool".to_string(),
            call_id: call_id.clone(),
            tool_type: "function".to_string(),
            call_arguments: captured_args,
            // Capture both successful and error results when result capture is
            // enabled — error payloads are often the most useful for debugging
            // tool failures, and they go through the same sanitize pipeline
            // (allowlist → custom redactor → default redactor → size limit).
            call_result: if s.tool_io_capture.captures_results() {
                Some(s.sanitize_tool_payload(&result.data))
            } else {
                None
            },
            error_type,
            duration_ms,
            started_at_ms,
            ended_at_ms,
        };

        if let Some(tracing_span) = tracing_span {
            if let Some(value) = &span.call_result
                && let Ok(serialized) = serde_json::to_string(value)
            {
                tracing_span.record("gen_ai.tool.call.result", serialized.as_str());
            }
            if span.has_truncated_payload() {
                tracing_span.record("awaken.tool.payload.truncated", true);
            }
            if let (Some(v), Some(msg)) = (&span.error_type, &error_message) {
                tracing_span.record("error.type", v.as_str());
                tracing_span.record("error.message", msg.as_str());
                tracing_span.record("otel.status_code", "ERROR");
                tracing_span.record("otel.status_description", msg.as_str());
            }
            drop(tracing_span);
        }

        crate::prometheus::record_tool(&span);
        s.sink.record(&MetricsEvent::Tool(span.clone()));
        s.metrics.lock().await.tools.push(span);

        // Detect tool suspension: ToolStatus::Pending means the tool suspended
        // (e.g., HITL approval, frontend tool, or permission gate).
        if result.status == ToolStatus::Pending {
            let context = s.span_context.lock().await.clone();
            let suspension = SuspensionSpan {
                context,
                tool_call_id: call_id.clone(),
                tool_name: result.tool_name.clone(),
                action: "suspended".to_string(),
                resume_mode: None,
                duration_ms: None,
                timestamp_ms: now_epoch_ms(),
            };
            crate::prometheus::record_suspension(&suspension);
            s.sink.record(&MetricsEvent::Suspension(suspension.clone()));
            s.metrics.lock().await.suspensions.push(suspension);
        }

        // Detect delegation: tool names prefixed with `agent_run_` come from
        // AgentTool, which delegates work to a sub-agent.
        if result.tool_name.starts_with(DELEGATION_TOOL_PREFIX) {
            let context = s.span_context.lock().await.clone();
            let target_agent_id = result
                .tool_name
                .strip_prefix(DELEGATION_TOOL_PREFIX)
                .unwrap_or_default()
                .to_string();
            let is_error = result.status == ToolStatus::Error;
            let child_run_id = result
                .metadata
                .get("child_run_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let delegation = DelegationSpan {
                context,
                parent_run_id: ctx.run_identity.run_id.clone(),
                child_run_id,
                target_agent_id,
                tool_call_id: call_id,
                duration_ms: Some(duration_ms),
                success: !is_error,
                error_message: if is_error {
                    result.message.clone()
                } else {
                    None
                },
                timestamp_ms: now_epoch_ms(),
            };
            crate::prometheus::record_delegation(&delegation);
            s.sink.record(&MetricsEvent::Delegation(delegation.clone()));
            s.metrics.lock().await.delegations.push(delegation);
        }

        Ok(StateCommand::new())
    }
}

pub(crate) struct RunEndHook(pub(crate) Arc<Inner>);

#[async_trait]
impl PhaseHook for RunEndHook {
    async fn run(&self, _ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        let s = &self.0;

        let session_duration_ms = s
            .run_start
            .lock()
            .await
            .take()
            .map(|start| start.elapsed().as_millis() as u64)
            .unwrap_or(0);

        s.inference_tracing_span.lock().await.take();
        s.tool_tracing_span.lock().await.clear();
        s.tool_start.lock().await.clear();

        let mut metrics = s.metrics.lock().await.clone();
        metrics.session_duration_ms = session_duration_ms;
        crate::prometheus::record_run_end(&metrics);
        s.sink.on_run_end(&metrics);
        *s.metrics.lock().await = crate::metrics::AgentMetrics::default();
        // See `RunStartHook` for why `background_task_statuses` is not cleared.

        Ok(StateCommand::new())
    }
}

pub(crate) struct BackgroundTaskObserveHook(pub(crate) Arc<Inner>);

#[async_trait]
impl PhaseHook for BackgroundTaskObserveHook {
    async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        let Some(snapshot) = ctx.state::<BackgroundTaskStateKey>() else {
            return Ok(StateCommand::new());
        };

        let s = &self.0;
        for meta in snapshot.tasks.values() {
            let status = meta.status;
            let key = (meta.owner_thread_id.clone(), meta.task_id.clone());
            let should_record = {
                let mut seen = s.background_task_statuses.lock().await;
                if seen.get(&key) == Some(&status) {
                    false
                } else {
                    seen.insert(key, status);
                    true
                }
            };

            if !should_record {
                continue;
            }

            let span = background_task_span_from_meta(meta);
            s.sink.record(&MetricsEvent::BackgroundTask(span.clone()));
            s.metrics.lock().await.background_tasks.push(span);
        }

        Ok(StateCommand::new())
    }
}

fn background_task_span_from_meta(meta: &PersistedTaskMeta) -> BackgroundTaskSpan {
    let parent = &meta.parent_context;
    BackgroundTaskSpan {
        context: SpanContext {
            run_id: parent.run_id.clone().unwrap_or_default(),
            thread_id: meta.owner_thread_id.clone(),
            agent_id: parent.agent_id.clone().unwrap_or_default(),
            parent_run_id: None,
            parent_tool_call_id: parent.call_id.clone(),
            ..Default::default()
        },
        task_id: meta.task_id.clone(),
        task_type: meta.task_type.clone(),
        task_name: meta.name.clone(),
        description: meta.description.clone(),
        status: meta.status,
        parent_task_id: meta.parent_context.task_id.clone(),
        error_message: meta.error.clone(),
        created_at_ms: meta.created_at_ms,
        completed_at_ms: meta.completed_at_ms,
    }
}
