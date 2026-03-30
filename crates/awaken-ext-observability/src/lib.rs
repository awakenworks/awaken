//! LLM telemetry plugin aligned with OpenTelemetry GenAI Semantic Conventions.
//!
//! Captures per-inference and per-tool metrics via the Phase system,
//! forwarding them to a pluggable [`MetricsSink`].

mod batching;
mod composite;
mod metrics;
mod persistent;
mod plugin;
mod sink;
mod stats;

#[cfg(feature = "otel")]
pub mod otel;
#[cfg(feature = "otel")]
mod otel_config;

pub use batching::{BatchingConfig, BatchingSink};
pub use composite::{CompositeSink, CompositeSinkBuilder};
pub use metrics::{
    AgentMetrics, DelegationSpan, GenAISpan, HandoffSpan, MetricsEvent, SuspensionSpan, ToolSpan,
};
pub use persistent::{PersistenceConfig, PersistentSink};
pub use plugin::{OBSERVABILITY_PLUGIN_ID, ObservabilityPlugin};
pub use sink::{InMemorySink, MetricsSink, SinkError};

#[cfg(feature = "otel")]
pub use otel::OtelMetricsSink;
#[cfg(feature = "otel")]
pub use otel_config::{OtelConfig, OtelConfigBuilder, OtelProtocol};
pub use stats::{ModelStats, ToolStats};

// Make private helpers visible to the test module below.
#[cfg(test)]
use plugin::{extract_cache_tokens, extract_token_counts};

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::inference::{
        InferenceError, LLMResponse, StreamResult, TokenUsage,
    };
    use awaken_contract::contract::tool::ToolResult;
    use awaken_contract::model::Phase;
    use awaken_contract::state::{Snapshot, StateMap};
    use awaken_runtime::{PhaseContext, PhaseHook};
    use futures::future::join_all;
    use std::sync::Arc;

    fn empty_snapshot() -> Snapshot {
        Snapshot::new(0, Arc::new(StateMap::default()))
    }

    /// Dispatch helper: invoke the appropriate phase hook sharing the plugin's inner state.
    async fn run_phase(plugin: &ObservabilityPlugin, ctx: &PhaseContext) {
        let inner = Arc::clone(&plugin.inner);
        match ctx.phase {
            Phase::RunStart => {
                plugin::RunStartHook(inner).run(ctx).await.unwrap();
            }
            Phase::BeforeInference => {
                plugin::BeforeInferenceHook(inner).run(ctx).await.unwrap();
            }
            Phase::AfterInference => {
                plugin::AfterInferenceHook(inner).run(ctx).await.unwrap();
            }
            Phase::BeforeToolExecute => {
                plugin::BeforeToolExecuteHook(inner).run(ctx).await.unwrap();
            }
            Phase::AfterToolExecute => {
                plugin::AfterToolExecuteHook(inner).run(ctx).await.unwrap();
            }
            Phase::RunEnd => {
                plugin::RunEndHook(inner).run(ctx).await.unwrap();
            }
            _ => {}
        }
    }

    fn usage(prompt: i32, completion: i32, total: i32) -> TokenUsage {
        TokenUsage {
            prompt_tokens: Some(prompt),
            completion_tokens: Some(completion),
            total_tokens: Some(total),
            cache_read_tokens: None,
            cache_creation_tokens: None,
            thinking_tokens: None,
        }
    }

    fn usage_with_cache(prompt: i32, completion: i32, total: i32, cached: i32) -> TokenUsage {
        TokenUsage {
            prompt_tokens: Some(prompt),
            completion_tokens: Some(completion),
            total_tokens: Some(total),
            cache_read_tokens: Some(cached),
            cache_creation_tokens: None,
            thinking_tokens: None,
        }
    }

    fn make_span(model: &str, provider: &str) -> GenAISpan {
        GenAISpan {
            model: model.into(),
            provider: provider.into(),
            operation: "chat".into(),
            response_model: None,
            response_id: None,
            finish_reasons: Vec::new(),
            error_type: None,
            error_class: None,
            thinking_tokens: None,
            input_tokens: Some(10),
            output_tokens: Some(20),
            total_tokens: Some(30),
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            temperature: None,
            top_p: None,
            max_tokens: None,
            stop_sequences: Vec::new(),
            duration_ms: 100,
        }
    }

    fn make_tool_span(name: &str, call_id: &str) -> ToolSpan {
        ToolSpan {
            name: name.into(),
            operation: "execute_tool".into(),
            call_id: call_id.into(),
            tool_type: "function".into(),
            error_type: None,
            duration_ms: 10,
        }
    }

    fn success_response(u: Option<TokenUsage>) -> LLMResponse {
        use awaken_contract::contract::content::ContentBlock;
        LLMResponse::success(StreamResult {
            content: vec![ContentBlock::text("hello")],
            tool_calls: vec![],
            usage: u,
            stop_reason: None,
            has_incomplete_tool_calls: false,
        })
    }

    // ---- ToolSpan::is_success ----

    #[test]
    fn test_tool_span_is_success() {
        let span = make_tool_span("search", "c1");
        assert!(span.is_success());

        let span = ToolSpan {
            error_type: Some("permission denied".into()),
            ..make_tool_span("write", "c2")
        };
        assert!(!span.is_success());
    }

    // ---- AgentMetrics ----

    #[test]
    fn test_agent_metrics_defaults() {
        let m = AgentMetrics::default();
        assert_eq!(m.total_input_tokens(), 0);
        assert_eq!(m.total_output_tokens(), 0);
        assert_eq!(m.total_tokens(), 0);
        assert_eq!(m.inference_count(), 0);
        assert_eq!(m.tool_count(), 0);
        assert_eq!(m.tool_failures(), 0);
    }

    #[test]
    fn test_agent_metrics_aggregation() {
        let m = AgentMetrics {
            inferences: vec![
                make_span("m", "openai"),
                GenAISpan {
                    input_tokens: Some(5),
                    output_tokens: None,
                    total_tokens: Some(8),
                    duration_ms: 50,
                    ..make_span("m", "openai")
                },
            ],
            tools: vec![
                make_tool_span("a", "c1"),
                ToolSpan {
                    error_type: Some("permission denied".into()),
                    ..make_tool_span("b", "c2")
                },
            ],
            session_duration_ms: 500,
            ..Default::default()
        };
        assert_eq!(m.total_input_tokens(), 15);
        assert_eq!(m.total_output_tokens(), 20);
        assert_eq!(m.total_tokens(), 38);
        assert_eq!(m.inference_count(), 2);
        assert_eq!(m.tool_count(), 2);
        assert_eq!(m.tool_failures(), 1);
    }

    // ---- InMemorySink ----

    #[test]
    fn test_in_memory_sink_collects() {
        let sink = InMemorySink::new();
        sink.record(&MetricsEvent::Inference(make_span("test", "openai")));
        sink.record(&MetricsEvent::Tool(make_tool_span("t", "c1")));
        let m = sink.metrics();
        assert_eq!(m.inference_count(), 1);
        assert_eq!(m.tool_count(), 1);
    }

    #[test]
    fn test_in_memory_sink_run_end() {
        let sink = InMemorySink::new();
        let metrics = AgentMetrics {
            session_duration_ms: 999,
            ..Default::default()
        };
        sink.on_run_end(&metrics);
        assert_eq!(sink.metrics().session_duration_ms, 999);
    }

    // ---- ObservabilityPlugin ----

    #[tokio::test]
    async fn test_plugin_captures_inference() {
        let sink = InMemorySink::new();
        let plugin = ObservabilityPlugin::new(sink.clone())
            .with_model("gpt-4")
            .with_provider("openai");

        let ctx = PhaseContext::new(Phase::BeforeInference, empty_snapshot());
        run_phase(&plugin, &ctx).await;

        let ctx = PhaseContext::new(Phase::AfterInference, empty_snapshot())
            .with_llm_response(success_response(Some(usage(100, 50, 150))));
        run_phase(&plugin, &ctx).await;

        let m = sink.metrics();
        assert_eq!(m.inference_count(), 1);
        assert_eq!(m.total_input_tokens(), 100);
        assert_eq!(m.total_output_tokens(), 50);
        assert_eq!(m.inferences[0].model, "gpt-4");
        assert_eq!(m.inferences[0].provider, "openai");
        assert_eq!(m.inferences[0].operation, "chat");
        assert!(m.inferences[0].cache_read_input_tokens.is_none());
    }

    #[tokio::test]
    async fn test_plugin_captures_inference_with_cache() {
        let sink = InMemorySink::new();
        let plugin = ObservabilityPlugin::new(sink.clone())
            .with_model("gpt-4")
            .with_provider("openai");

        let ctx = PhaseContext::new(Phase::BeforeInference, empty_snapshot());
        run_phase(&plugin, &ctx).await;

        let ctx = PhaseContext::new(Phase::AfterInference, empty_snapshot())
            .with_llm_response(success_response(Some(usage_with_cache(100, 50, 150, 30))));
        run_phase(&plugin, &ctx).await;

        let m = sink.metrics();
        let span = &m.inferences[0];
        assert_eq!(span.cache_read_input_tokens, Some(30));
        assert!(span.cache_creation_input_tokens.is_none());
    }

    #[tokio::test]
    async fn test_plugin_captures_tool() {
        let sink = InMemorySink::new();
        let plugin = ObservabilityPlugin::new(sink.clone());

        let ctx = PhaseContext::new(Phase::BeforeToolExecute, empty_snapshot()).with_tool_info(
            "search",
            "c1",
            Some(serde_json::json!({})),
        );
        run_phase(&plugin, &ctx).await;

        let ctx = PhaseContext::new(Phase::AfterToolExecute, empty_snapshot())
            .with_tool_info("search", "c1", Some(serde_json::json!({})))
            .with_tool_result(ToolResult::success(
                "search",
                serde_json::json!({"found": true}),
            ));
        run_phase(&plugin, &ctx).await;

        let m = sink.metrics();
        assert_eq!(m.tool_count(), 1);
        assert!(m.tools[0].is_success());
        assert_eq!(m.tools[0].name, "search");
        assert_eq!(m.tools[0].call_id, "c1");
        assert_eq!(m.tools[0].operation, "execute_tool");
        assert!(m.tools[0].error_type.is_none());
    }

    #[tokio::test]
    async fn test_plugin_captures_tool_failure() {
        let sink = InMemorySink::new();
        let plugin = ObservabilityPlugin::new(sink.clone());

        let ctx = PhaseContext::new(Phase::BeforeToolExecute, empty_snapshot()).with_tool_info(
            "write",
            "c1",
            Some(serde_json::json!({})),
        );
        run_phase(&plugin, &ctx).await;

        let ctx = PhaseContext::new(Phase::AfterToolExecute, empty_snapshot())
            .with_tool_info("write", "c1", Some(serde_json::json!({})))
            .with_tool_result(ToolResult::error("write", "permission denied"));
        run_phase(&plugin, &ctx).await;

        let m = sink.metrics();
        assert!(!m.tools[0].is_success());
        assert_eq!(m.tools[0].error_type.as_deref(), Some("tool_error"));
    }

    #[tokio::test]
    async fn test_plugin_session_lifecycle() {
        let sink = InMemorySink::new();
        let plugin = ObservabilityPlugin::new(sink.clone());

        let ctx = PhaseContext::new(Phase::RunStart, empty_snapshot());
        run_phase(&plugin, &ctx).await;

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        let ctx = PhaseContext::new(Phase::RunEnd, empty_snapshot());
        run_phase(&plugin, &ctx).await;

        let m = sink.metrics();
        assert!(m.session_duration_ms >= 10);
    }

    #[tokio::test]
    async fn test_plugin_no_usage() {
        let sink = InMemorySink::new();
        let plugin = ObservabilityPlugin::new(sink.clone()).with_model("m");

        let ctx = PhaseContext::new(Phase::BeforeInference, empty_snapshot());
        run_phase(&plugin, &ctx).await;

        let ctx = PhaseContext::new(Phase::AfterInference, empty_snapshot())
            .with_llm_response(success_response(None));
        run_phase(&plugin, &ctx).await;

        let m = sink.metrics();
        assert_eq!(m.inference_count(), 1);
        assert!(m.inferences[0].input_tokens.is_none());
        assert!(m.inferences[0].cache_read_input_tokens.is_none());
    }

    #[tokio::test]
    async fn test_plugin_multiple_rounds() {
        let sink = InMemorySink::new();
        let plugin = ObservabilityPlugin::new(sink.clone()).with_model("m");

        for i in 0..3 {
            let ctx = PhaseContext::new(Phase::BeforeInference, empty_snapshot());
            run_phase(&plugin, &ctx).await;

            let ctx = PhaseContext::new(Phase::AfterInference, empty_snapshot()).with_llm_response(
                success_response(Some(usage(10 * (i + 1), 5 * (i + 1), 15 * (i + 1)))),
            );
            run_phase(&plugin, &ctx).await;
        }

        let m = sink.metrics();
        assert_eq!(m.inference_count(), 3);
        assert_eq!(m.total_input_tokens(), 60); // 10+20+30
        assert_eq!(m.total_output_tokens(), 30); // 5+10+15
    }

    #[tokio::test]
    async fn test_plugin_captures_inference_error() {
        let sink = InMemorySink::new();
        let plugin = ObservabilityPlugin::new(sink.clone())
            .with_model("gpt-4")
            .with_provider("openai");

        let ctx = PhaseContext::new(Phase::BeforeInference, empty_snapshot());
        run_phase(&plugin, &ctx).await;

        let ctx = PhaseContext::new(Phase::AfterInference, empty_snapshot()).with_llm_response(
            LLMResponse::error(InferenceError {
                error_type: "rate_limited".to_string(),
                message: "429".to_string(),
                error_class: Some("rate_limit".to_string()),
            }),
        );
        run_phase(&plugin, &ctx).await;

        let m = sink.metrics();
        assert_eq!(m.inference_count(), 1);
        assert_eq!(m.inferences[0].error_type.as_deref(), Some("rate_limited"));
    }

    #[tokio::test]
    async fn test_plugin_parallel_tool_spans_are_isolated_by_call_id() {
        use std::time::Duration;

        let sink = InMemorySink::new();
        let plugin = Arc::new(ObservabilityPlugin::new(sink.clone()).with_provider("p"));

        let calls = vec![("search", "c1"), ("write", "c2"), ("read", "c3")];

        let tasks = calls.into_iter().enumerate().map(|(i, (name, id))| {
            let plugin = Arc::clone(&plugin);
            let name = name.to_string();
            let id = id.to_string();
            async move {
                let ctx = PhaseContext::new(Phase::BeforeToolExecute, empty_snapshot())
                    .with_tool_info(name.as_str(), id.as_str(), Some(serde_json::json!({})));
                run_phase(&plugin, &ctx).await;

                // Stagger completion to maximize the chance of cross-talk.
                tokio::time::sleep(Duration::from_millis(5 * (3 - i) as u64)).await;

                let ctx = PhaseContext::new(Phase::AfterToolExecute, empty_snapshot())
                    .with_tool_info(name.as_str(), id.as_str(), Some(serde_json::json!({})))
                    .with_tool_result(ToolResult::success(
                        name.as_str(),
                        serde_json::json!({"ok": true}),
                    ));
                run_phase(&plugin, &ctx).await;
            }
        });

        join_all(tasks).await;

        let m = sink.metrics();
        assert_eq!(m.tool_count(), 3);
        let mut ids: Vec<String> = m.tools.iter().map(|t| t.call_id.clone()).collect();
        ids.sort();
        assert_eq!(ids, vec!["c1", "c2", "c3"]);
    }

    #[test]
    fn test_genai_span_serialization() {
        let span = make_span("gpt-4", "openai");
        let json = serde_json::to_value(&span).unwrap();
        assert_eq!(json["model"], "gpt-4");
        assert_eq!(json["input_tokens"], 10);
        assert_eq!(json["provider"], "openai");
        assert_eq!(json["operation"], "chat");
    }

    #[test]
    fn test_tool_span_serialization() {
        let span = make_tool_span("search", "c1");
        let json = serde_json::to_value(&span).unwrap();
        assert_eq!(json["name"], "search");
        assert_eq!(json["call_id"], "c1");
        assert_eq!(json["operation"], "execute_tool");
    }

    #[test]
    fn test_agent_metrics_serialization() {
        let m = AgentMetrics::default();
        let json = serde_json::to_string(&m).unwrap();
        let m2: AgentMetrics = serde_json::from_str(&json).unwrap();
        assert_eq!(m2.session_duration_ms, 0);
    }

    #[test]
    fn test_extract_token_counts_some() {
        let u = usage(10, 20, 30);
        let (i, o, t, thinking) = extract_token_counts(Some(&u));
        assert_eq!(i, Some(10));
        assert_eq!(o, Some(20));
        assert_eq!(t, Some(30));
        assert_eq!(thinking, None);
    }

    #[test]
    fn test_extract_token_counts_with_thinking() {
        let u = TokenUsage {
            prompt_tokens: Some(10),
            completion_tokens: Some(20),
            total_tokens: Some(30),
            cache_read_tokens: None,
            cache_creation_tokens: None,
            thinking_tokens: Some(7),
        };
        let (i, o, t, thinking) = extract_token_counts(Some(&u));
        assert_eq!(i, Some(10));
        assert_eq!(o, Some(20));
        assert_eq!(t, Some(30));
        assert_eq!(thinking, Some(7));
    }

    #[test]
    fn test_extract_token_counts_none() {
        let (i, o, t, thinking) = extract_token_counts(None);
        assert!(i.is_none());
        assert!(o.is_none());
        assert!(t.is_none());
        assert!(thinking.is_none());
    }

    #[test]
    fn test_extract_cache_tokens() {
        let u = usage_with_cache(100, 50, 150, 30);
        let (read, creation) = extract_cache_tokens(Some(&u));
        assert_eq!(read, Some(30));
        assert!(creation.is_none());
    }

    #[test]
    fn test_extract_cache_tokens_none() {
        assert_eq!(extract_cache_tokens(None), (None, None));
        let u = usage(10, 20, 30);
        assert_eq!(extract_cache_tokens(Some(&u)), (None, None));
    }

    // ---- stats_by_model ----

    #[test]
    fn test_stats_by_model_empty() {
        let m = AgentMetrics::default();
        assert!(m.stats_by_model().is_empty());
    }

    #[test]
    fn test_stats_by_model_single() {
        let m = AgentMetrics {
            inferences: vec![
                make_span("gpt-4", "openai"),
                GenAISpan {
                    input_tokens: Some(5),
                    output_tokens: Some(3),
                    total_tokens: Some(8),
                    duration_ms: 50,
                    ..make_span("gpt-4", "openai")
                },
            ],
            ..Default::default()
        };
        let stats = m.stats_by_model();
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].model, "gpt-4");
        assert_eq!(stats[0].provider, "openai");
        assert_eq!(stats[0].inference_count, 2);
        assert_eq!(stats[0].input_tokens, 15);
        assert_eq!(stats[0].output_tokens, 23);
        assert_eq!(stats[0].total_tokens, 38);
        assert_eq!(stats[0].total_duration_ms, 150);
    }

    #[test]
    fn test_stats_by_model_multiple() {
        let m = AgentMetrics {
            inferences: vec![
                make_span("gpt-4", "openai"),
                make_span("claude-3", "anthropic"),
                GenAISpan {
                    input_tokens: Some(50),
                    output_tokens: Some(25),
                    total_tokens: Some(75),
                    duration_ms: 200,
                    ..make_span("claude-3", "anthropic")
                },
            ],
            ..Default::default()
        };
        let stats = m.stats_by_model();
        assert_eq!(stats.len(), 2);
        // Sorted by model name
        assert_eq!(stats[0].model, "claude-3");
        assert_eq!(stats[0].inference_count, 2);
        assert_eq!(stats[0].input_tokens, 60);
        assert_eq!(stats[0].output_tokens, 45);
        assert_eq!(stats[0].total_duration_ms, 300);

        assert_eq!(stats[1].model, "gpt-4");
        assert_eq!(stats[1].inference_count, 1);
    }

    #[test]
    fn test_stats_by_model_with_cache_tokens() {
        let m = AgentMetrics {
            inferences: vec![GenAISpan {
                cache_read_input_tokens: Some(30),
                cache_creation_input_tokens: Some(10),
                ..make_span("claude-3", "anthropic")
            }],
            ..Default::default()
        };
        let stats = m.stats_by_model();
        assert_eq!(stats[0].cache_read_input_tokens, 30);
        assert_eq!(stats[0].cache_creation_input_tokens, 10);
    }

    // ---- stats_by_tool ----

    #[test]
    fn test_stats_by_tool_empty() {
        let m = AgentMetrics::default();
        assert!(m.stats_by_tool().is_empty());
    }

    #[test]
    fn test_stats_by_tool_single() {
        let m = AgentMetrics {
            tools: vec![
                make_tool_span("search", "c1"),
                ToolSpan {
                    duration_ms: 20,
                    ..make_tool_span("search", "c2")
                },
            ],
            ..Default::default()
        };
        let stats = m.stats_by_tool();
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].name, "search");
        assert_eq!(stats[0].call_count, 2);
        assert_eq!(stats[0].failure_count, 0);
        assert_eq!(stats[0].total_duration_ms, 30);
    }

    #[test]
    fn test_stats_by_tool_multiple() {
        let m = AgentMetrics {
            tools: vec![
                make_tool_span("search", "c1"),
                make_tool_span("write", "c2"),
                make_tool_span("search", "c3"),
            ],
            ..Default::default()
        };
        let stats = m.stats_by_tool();
        assert_eq!(stats.len(), 2);
        // Sorted by name
        assert_eq!(stats[0].name, "search");
        assert_eq!(stats[0].call_count, 2);
        assert_eq!(stats[1].name, "write");
        assert_eq!(stats[1].call_count, 1);
    }

    #[test]
    fn test_stats_by_tool_with_failures() {
        let m = AgentMetrics {
            tools: vec![
                make_tool_span("write", "c1"),
                ToolSpan {
                    error_type: Some("permission denied".into()),
                    ..make_tool_span("write", "c2")
                },
                ToolSpan {
                    error_type: Some("not found".into()),
                    ..make_tool_span("write", "c3")
                },
            ],
            ..Default::default()
        };
        let stats = m.stats_by_tool();
        assert_eq!(stats[0].call_count, 3);
        assert_eq!(stats[0].failure_count, 2);
    }

    // ---- total cache/duration methods ----

    #[test]
    fn test_total_cache_tokens() {
        let m = AgentMetrics {
            inferences: vec![
                GenAISpan {
                    cache_read_input_tokens: Some(30),
                    cache_creation_input_tokens: Some(10),
                    ..make_span("m", "p")
                },
                GenAISpan {
                    cache_read_input_tokens: Some(20),
                    cache_creation_input_tokens: None,
                    ..make_span("m", "p")
                },
            ],
            ..Default::default()
        };
        assert_eq!(m.total_cache_read_tokens(), 50);
        assert_eq!(m.total_cache_creation_tokens(), 10);
    }

    #[test]
    fn test_total_duration_methods() {
        let m = AgentMetrics {
            inferences: vec![
                make_span("m", "p"), // 100ms
                GenAISpan {
                    duration_ms: 200,
                    ..make_span("m", "p")
                },
            ],
            tools: vec![
                make_tool_span("a", "c1"), // 10ms
                ToolSpan {
                    duration_ms: 30,
                    ..make_tool_span("b", "c2")
                },
            ],
            ..Default::default()
        };
        assert_eq!(m.total_inference_duration_ms(), 300);
        assert_eq!(m.total_tool_duration_ms(), 40);
    }

    // ---- Tracing span capture tests ----

    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::registry::LookupSpan;

    #[derive(Debug, Clone)]
    struct CapturedSpan {
        name: String,
        was_closed: bool,
    }

    struct SpanCaptureLayer {
        captured: Arc<std::sync::Mutex<Vec<CapturedSpan>>>,
    }

    impl<S: tracing::Subscriber + for<'a> LookupSpan<'a>> tracing_subscriber::Layer<S>
        for SpanCaptureLayer
    {
        fn on_new_span(
            &self,
            _attrs: &tracing::span::Attributes<'_>,
            id: &tracing::span::Id,
            ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if let Some(span_ref) = ctx.span(id) {
                self.captured.lock().unwrap().push(CapturedSpan {
                    name: span_ref.name().to_string(),
                    was_closed: false,
                });
            }
        }

        fn on_close(&self, id: tracing::span::Id, ctx: tracing_subscriber::layer::Context<'_, S>) {
            if let Some(span_ref) = ctx.span(&id) {
                let name = span_ref.name().to_string();
                let mut captured = self.captured.lock().unwrap();
                if let Some(entry) = captured.iter_mut().find(|c| c.name == name) {
                    entry.was_closed = true;
                }
            }
        }
    }

    #[tokio::test]
    async fn test_tracing_span_inference() {
        let captured = Arc::new(std::sync::Mutex::new(Vec::<CapturedSpan>::new()));
        let layer = SpanCaptureLayer {
            captured: captured.clone(),
        };
        let subscriber = tracing_subscriber::registry::Registry::default().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        let sink = InMemorySink::new();
        let plugin = ObservabilityPlugin::new(sink.clone())
            .with_model("test-model")
            .with_provider("test-provider");

        let ctx = PhaseContext::new(Phase::BeforeInference, empty_snapshot());
        run_phase(&plugin, &ctx).await;

        let ctx = PhaseContext::new(Phase::AfterInference, empty_snapshot())
            .with_llm_response(success_response(Some(usage(10, 20, 30))));
        run_phase(&plugin, &ctx).await;

        let spans = captured.lock().unwrap();
        let inference_span = spans.iter().find(|s| s.name == "gen_ai");
        assert!(inference_span.is_some(), "expected gen_ai span (inference)");
        assert!(inference_span.unwrap().was_closed, "span should be closed");
    }

    #[tokio::test]
    async fn test_tracing_span_tool() {
        let captured = Arc::new(std::sync::Mutex::new(Vec::<CapturedSpan>::new()));
        let layer = SpanCaptureLayer {
            captured: captured.clone(),
        };
        let subscriber = tracing_subscriber::registry::Registry::default().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        let sink = InMemorySink::new();
        let plugin = ObservabilityPlugin::new(sink.clone());

        let ctx = PhaseContext::new(Phase::BeforeToolExecute, empty_snapshot()).with_tool_info(
            "search",
            "c1",
            Some(serde_json::json!({})),
        );
        run_phase(&plugin, &ctx).await;

        let ctx = PhaseContext::new(Phase::AfterToolExecute, empty_snapshot())
            .with_tool_info("search", "c1", Some(serde_json::json!({})))
            .with_tool_result(ToolResult::success(
                "search",
                serde_json::json!({"found": true}),
            ));
        run_phase(&plugin, &ctx).await;

        let spans = captured.lock().unwrap();
        let tool_span = spans.iter().find(|s| s.name == "gen_ai");
        assert!(tool_span.is_some(), "expected gen_ai span (tool)");
        assert!(tool_span.unwrap().was_closed, "span should be closed");
    }

    #[test]
    fn test_plugin_descriptor() {
        use awaken_runtime::Plugin;
        let sink = InMemorySink::new();
        let plugin = ObservabilityPlugin::new(sink);
        assert_eq!(Plugin::descriptor(&plugin).name, "observability");
    }

    // ---- Request parameter tests ----

    #[tokio::test]
    async fn test_plugin_captures_request_params() {
        let sink = InMemorySink::new();
        let plugin = ObservabilityPlugin::new(sink.clone())
            .with_model("m")
            .with_provider("p")
            .with_temperature(0.7)
            .with_max_tokens(2048)
            .with_top_p(0.9);

        let ctx = PhaseContext::new(Phase::BeforeInference, empty_snapshot());
        run_phase(&plugin, &ctx).await;

        let ctx = PhaseContext::new(Phase::AfterInference, empty_snapshot())
            .with_llm_response(success_response(Some(usage(10, 5, 15))));
        run_phase(&plugin, &ctx).await;

        let m = sink.metrics();
        let span = &m.inferences[0];
        assert_eq!(span.temperature, Some(0.7));
        assert_eq!(span.top_p, Some(0.9));
        assert_eq!(span.max_tokens, Some(2048));
        assert!(span.stop_sequences.is_empty());
    }

    #[tokio::test]
    async fn test_plugin_captures_stop_sequences() {
        let sink = InMemorySink::new();
        let plugin = ObservabilityPlugin::new(sink.clone())
            .with_model("m")
            .with_stop_sequences(vec!["STOP".into(), "END".into()]);

        let ctx = PhaseContext::new(Phase::BeforeInference, empty_snapshot());
        run_phase(&plugin, &ctx).await;

        let ctx = PhaseContext::new(Phase::AfterInference, empty_snapshot())
            .with_llm_response(success_response(Some(usage(10, 5, 15))));
        run_phase(&plugin, &ctx).await;

        let m = sink.metrics();
        assert_eq!(m.inferences[0].stop_sequences, vec!["STOP", "END"]);
    }

    // ---- GenAISpan deserialization ----

    #[test]
    fn test_genai_span_deserialization_roundtrip() {
        let span = make_span("gpt-4", "openai");
        let json = serde_json::to_string(&span).unwrap();
        let parsed: GenAISpan = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.model, "gpt-4");
        assert_eq!(parsed.input_tokens, Some(10));
    }

    #[test]
    fn test_tool_span_deserialization_roundtrip() {
        let span = make_tool_span("search", "c1");
        let json = serde_json::to_string(&span).unwrap();
        let parsed: ToolSpan = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.name, "search");
        assert_eq!(parsed.call_id, "c1");
    }

    // ---- ModelStats / ToolStats ----

    #[test]
    fn test_model_stats_default() {
        let s = ModelStats::default();
        assert_eq!(s.inference_count, 0);
        assert_eq!(s.input_tokens, 0);
    }

    #[test]
    fn test_tool_stats_default() {
        let s = ToolStats::default();
        assert_eq!(s.call_count, 0);
        assert_eq!(s.failure_count, 0);
    }

    #[test]
    fn test_model_stats_serialization() {
        let s = ModelStats {
            model: "gpt-4".into(),
            provider: "openai".into(),
            inference_count: 2,
            input_tokens: 100,
            output_tokens: 50,
            total_tokens: 150,
            cache_read_input_tokens: 30,
            cache_creation_input_tokens: 10,
            total_duration_ms: 500,
        };
        let json = serde_json::to_string(&s).unwrap();
        let parsed: ModelStats = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.model, "gpt-4");
        assert_eq!(parsed.inference_count, 2);
    }

    #[test]
    fn test_tool_stats_serialization() {
        let s = ToolStats {
            name: "search".into(),
            call_count: 5,
            failure_count: 1,
            total_duration_ms: 250,
        };
        let json = serde_json::to_string(&s).unwrap();
        let parsed: ToolStats = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.name, "search");
        assert_eq!(parsed.call_count, 5);
    }

    // ---- after_tool_execute with no tool result ----

    #[tokio::test]
    async fn test_after_tool_execute_no_result_is_noop() {
        let sink = InMemorySink::new();
        let plugin = ObservabilityPlugin::new(sink.clone());

        let ctx = PhaseContext::new(Phase::AfterToolExecute, empty_snapshot())
            .with_tool_info("search", "c1", None);
        run_phase(&plugin, &ctx).await;

        assert_eq!(sink.metrics().tool_count(), 0);
    }

    // ---- InMemorySink thread safety ----

    #[test]
    fn test_in_memory_sink_is_clone() {
        let sink = InMemorySink::new();
        let sink2 = sink.clone();
        sink.record(&MetricsEvent::Inference(make_span("m", "p")));
        assert_eq!(sink2.metrics().inference_count(), 1);
    }

    // ---- Multiple sinks ----

    #[test]
    fn test_metrics_sink_trait_object() {
        let sink: Box<dyn MetricsSink> = Box::new(InMemorySink::new());
        sink.record(&MetricsEvent::Inference(make_span("m", "p")));
        sink.record(&MetricsEvent::Tool(make_tool_span("t", "c1")));
        sink.on_run_end(&AgentMetrics::default());
    }

    // ---- Inference error with class ----

    #[tokio::test]
    async fn test_plugin_captures_inference_error_with_class() {
        let sink = InMemorySink::new();
        let plugin = ObservabilityPlugin::new(sink.clone())
            .with_model("m")
            .with_provider("p");

        let ctx = PhaseContext::new(Phase::BeforeInference, empty_snapshot());
        run_phase(&plugin, &ctx).await;

        let ctx = PhaseContext::new(Phase::AfterInference, empty_snapshot()).with_llm_response(
            LLMResponse::error(InferenceError {
                error_type: "timeout".to_string(),
                message: "connection timed out".to_string(),
                error_class: Some("connection".to_string()),
            }),
        );
        run_phase(&plugin, &ctx).await;

        let m = sink.metrics();
        assert_eq!(m.inferences[0].error_type.as_deref(), Some("timeout"));
        assert_eq!(m.inferences[0].error_class.as_deref(), Some("connection"));
    }

    // ---- Duration is measured ----

    #[tokio::test]
    async fn test_inference_duration_is_measured() {
        let sink = InMemorySink::new();
        let plugin = ObservabilityPlugin::new(sink.clone()).with_model("m");

        let ctx = PhaseContext::new(Phase::BeforeInference, empty_snapshot());
        run_phase(&plugin, &ctx).await;

        tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;

        let ctx = PhaseContext::new(Phase::AfterInference, empty_snapshot())
            .with_llm_response(success_response(None));
        run_phase(&plugin, &ctx).await;

        let m = sink.metrics();
        assert!(m.inferences[0].duration_ms >= 5);
    }

    #[tokio::test]
    async fn test_tool_duration_is_measured() {
        let sink = InMemorySink::new();
        let plugin = ObservabilityPlugin::new(sink.clone());

        let ctx = PhaseContext::new(Phase::BeforeToolExecute, empty_snapshot())
            .with_tool_info("search", "c1", None);
        run_phase(&plugin, &ctx).await;

        tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;

        let ctx = PhaseContext::new(Phase::AfterToolExecute, empty_snapshot())
            .with_tool_info("search", "c1", None)
            .with_tool_result(ToolResult::success("search", serde_json::json!({})));
        run_phase(&plugin, &ctx).await;

        let m = sink.metrics();
        assert!(m.tools[0].duration_ms >= 5);
    }

    // ---- No LLM response in AfterInference ----

    #[tokio::test]
    async fn test_after_inference_no_response() {
        let sink = InMemorySink::new();
        let plugin = ObservabilityPlugin::new(sink.clone()).with_model("m");

        let ctx = PhaseContext::new(Phase::BeforeInference, empty_snapshot());
        run_phase(&plugin, &ctx).await;

        // AfterInference with no LLM response at all
        let ctx = PhaseContext::new(Phase::AfterInference, empty_snapshot());
        run_phase(&plugin, &ctx).await;

        let m = sink.metrics();
        assert_eq!(m.inference_count(), 1);
        assert!(m.inferences[0].input_tokens.is_none());
        assert!(m.inferences[0].error_type.is_none());
    }

    // ---- Tool type field ----

    #[tokio::test]
    async fn test_tool_span_has_function_type() {
        let sink = InMemorySink::new();
        let plugin = ObservabilityPlugin::new(sink.clone());

        let ctx = PhaseContext::new(Phase::BeforeToolExecute, empty_snapshot())
            .with_tool_info("search", "c1", None);
        run_phase(&plugin, &ctx).await;

        let ctx = PhaseContext::new(Phase::AfterToolExecute, empty_snapshot())
            .with_tool_info("search", "c1", None)
            .with_tool_result(ToolResult::success("search", serde_json::json!({})));
        run_phase(&plugin, &ctx).await;

        let m = sink.metrics();
        assert_eq!(m.tools[0].tool_type, "function");
    }
}
