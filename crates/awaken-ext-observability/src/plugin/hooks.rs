use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use awaken_contract::StateError;
use awaken_contract::contract::tool::ToolStatus;
use awaken_runtime::{PhaseContext, PhaseHook, StateCommand};

use crate::metrics::{GenAISpan, MetricsEvent, ToolSpan};

use super::shared::{Inner, extract_cache_tokens, extract_token_counts};

pub(crate) struct RunStartHook(pub(crate) Arc<Inner>);

#[async_trait]
impl PhaseHook for RunStartHook {
    async fn run(&self, _ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        *self.0.run_start.lock().await = Some(Instant::now());
        Ok(StateCommand::new())
    }
}

pub(crate) struct BeforeInferenceHook(pub(crate) Arc<Inner>);

#[async_trait]
impl PhaseHook for BeforeInferenceHook {
    async fn run(&self, _ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        let s = &self.0;

        // Close any abandoned inference tracing span from a retried attempt.
        if let Some(previous_span) = s.inference_tracing_span.lock().await.take() {
            let message = "A previous inference attempt was retried before completion.";
            previous_span.record("error.type", "inference_retry_interrupted");
            previous_span.record("error.message", message);
            previous_span.record("otel.status_code", "ERROR");
            previous_span.record("otel.status_description", message);
            drop(previous_span);
        }

        *s.inference_start.lock().await = Some(Instant::now());

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
            "gen_ai.usage.thinking_tokens" = tracing::field::Empty,
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

        let duration_ms = s
            .inference_start
            .lock()
            .await
            .take()
            .map(|start| start.elapsed().as_millis() as u64)
            .unwrap_or(0);

        // Extract usage and error from the LLM response.
        let (usage, error) = match &ctx.llm_response {
            Some(resp) => match &resp.outcome {
                Ok(result) => (result.usage.as_ref(), None),
                Err(err) => (None, Some(err)),
            },
            None => (None, None),
        };

        let (input_tokens, output_tokens, total_tokens, thinking_tokens) =
            extract_token_counts(usage);
        let (cache_read_input_tokens, cache_creation_input_tokens) = extract_cache_tokens(usage);

        let model = s.model.lock().await.clone();
        let provider = s.provider.lock().await.clone();
        let span = GenAISpan {
            model,
            provider,
            operation: s.operation.clone(),
            response_model: None,
            response_id: None,
            finish_reasons: Vec::new(),
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
        };

        // Record tracing span attributes.
        if let Some(tracing_span) = s.inference_tracing_span.lock().await.take() {
            if let Some(v) = span.thinking_tokens {
                tracing_span.record("gen_ai.usage.thinking_tokens", v);
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
                .insert(call_id.clone(), Instant::now());
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
            "error.type" = tracing::field::Empty,
            "error.message" = tracing::field::Empty,
        );

        if !call_id.is_empty() {
            s.tool_tracing_span.lock().await.insert(call_id, span);
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
        let duration_ms = s
            .tool_start
            .lock()
            .await
            .remove(&call_id)
            .map(|start| start.elapsed().as_millis() as u64)
            .unwrap_or(0);

        let Some(result) = ctx.tool_result.as_ref() else {
            return Ok(StateCommand::new());
        };

        let error_type = if result.status == ToolStatus::Error {
            Some("tool_error".to_string())
        } else {
            None
        };
        let error_message = result.message.clone().filter(|_| error_type.is_some());

        let span = ToolSpan {
            name: result.tool_name.clone(),
            operation: "execute_tool".to_string(),
            call_id: call_id.clone(),
            tool_type: "function".to_string(),
            error_type,
            duration_ms,
        };

        let tracing_span = s.tool_tracing_span.lock().await.remove(&call_id);
        if let Some(tracing_span) = tracing_span {
            if let (Some(v), Some(msg)) = (&span.error_type, &error_message) {
                tracing_span.record("error.type", v.as_str());
                tracing_span.record("error.message", msg.as_str());
                tracing_span.record("otel.status_code", "ERROR");
                tracing_span.record("otel.status_description", msg.as_str());
            }
            drop(tracing_span);
        }

        s.sink.record(&MetricsEvent::Tool(span.clone()));
        s.metrics.lock().await.tools.push(span);

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
        s.sink.on_run_end(&metrics);

        Ok(StateCommand::new())
    }
}
