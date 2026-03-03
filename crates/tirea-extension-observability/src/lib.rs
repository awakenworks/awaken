//! LLM telemetry plugin aligned with OpenTelemetry GenAI Semantic Conventions.
//!
//! Captures per-inference and per-tool metrics via the Phase system,
//! forwarding them to a pluggable [`MetricsSink`].

mod metrics;
mod plugin;
mod sink;
mod spans;

pub use metrics::{AgentMetrics, ModelStats, ToolStats};
pub use plugin::LLMMetryPlugin;
pub use sink::{InMemorySink, MetricsSink};
pub use spans::{GenAISpan, ToolSpan};

// Make private helpers visible to the test module below.
#[cfg(test)]
use plugin::{extract_cache_tokens, extract_token_counts};

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future::join_all;
    use serde_json::json;
    use std::sync::Arc;
    use tirea_contract::runtime::inference::{InferenceError, LLMResponse};
    use tirea_contract::runtime::phase::{Phase, StepContext};
    use tirea_contract::runtime::tool_call::ToolGate;
    use tirea_contract::runtime::tool_call::ToolResult;
    use tirea_contract::runtime::StreamResult;
    use tirea_contract::testing::TestFixture;
    use tirea_contract::thread::ToolCall;
    use tirea_contract::TokenUsage;

    /// Dispatch helper that builds a ReadOnlyContext from StepContext + fixture
    /// and calls the appropriate AgentBehavior hook.
    async fn run_phase(
        plugin: &(impl tirea_contract::AgentBehavior + ?Sized),
        phase: Phase,
        step: &StepContext<'_>,
        fixture: &TestFixture,
    ) {
        use tirea_contract::ReadOnlyContext;

        let config = &fixture.run_config;
        let doc = &fixture.doc;
        let messages = step.messages();
        let thread_id = step.thread_id();

        let mut ctx = ReadOnlyContext::new(phase, thread_id, messages, config, doc);

        if let Some(response) = step.extensions.get::<LLMResponse>() {
            ctx = ctx.with_llm_response(response);
        }

        if let Some(ref gate) = step.extensions.get::<ToolGate>() {
            ctx = ctx.with_tool_info(gate.name.as_str(), gate.id.as_str(), Some(&gate.args));
            if let Some(ref result) = gate.result {
                ctx = ctx.with_tool_result(result);
            }
        }

        match phase {
            Phase::RunStart => {
                tirea_contract::AgentBehavior::run_start(plugin, &ctx).await;
            }
            Phase::StepStart => {
                tirea_contract::AgentBehavior::step_start(plugin, &ctx).await;
            }
            Phase::BeforeInference => {
                tirea_contract::AgentBehavior::before_inference(plugin, &ctx).await;
            }
            Phase::AfterInference => {
                tirea_contract::AgentBehavior::after_inference(plugin, &ctx).await;
            }
            Phase::BeforeToolExecute => {
                tirea_contract::AgentBehavior::before_tool_execute(plugin, &ctx).await;
            }
            Phase::AfterToolExecute => {
                tirea_contract::AgentBehavior::after_tool_execute(plugin, &ctx).await;
            }
            Phase::StepEnd => {
                tirea_contract::AgentBehavior::step_end(plugin, &ctx).await;
            }
            Phase::RunEnd => {
                tirea_contract::AgentBehavior::run_end(plugin, &ctx).await;
            }
        }
    }

    fn usage(prompt: i32, completion: i32, total: i32) -> TokenUsage {
        TokenUsage {
            prompt_tokens: Some(prompt),
            completion_tokens: Some(completion),
            total_tokens: Some(total),
            cache_read_tokens: None,
            cache_creation_tokens: None,
        }
    }

    fn usage_with_cache(prompt: i32, completion: i32, total: i32, cached: i32) -> TokenUsage {
        TokenUsage {
            prompt_tokens: Some(prompt),
            completion_tokens: Some(completion),
            total_tokens: Some(total),
            cache_read_tokens: Some(cached),
            cache_creation_tokens: None,
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
        sink.on_inference(&make_span("test", "openai"));
        sink.on_tool(&make_tool_span("t", "c1"));
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

    // ---- LLMMetryPlugin ----

    #[tokio::test]
    async fn test_plugin_captures_inference() {
        let fix = TestFixture::new();
        let sink = InMemorySink::new();
        let plugin = LLMMetryPlugin::new(sink.clone())
            .with_model("gpt-4")
            .with_provider("openai");

        let mut step = fix.step(vec![]);

        run_phase(&plugin, Phase::BeforeInference, &step, &fix).await;

        step.extensions.insert(LLMResponse::success(StreamResult {
            text: "hello".into(),
            tool_calls: vec![],
            usage: Some(usage(100, 50, 150)),
        }));

        run_phase(&plugin, Phase::AfterInference, &step, &fix).await;

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
        let fix = TestFixture::new();
        let sink = InMemorySink::new();
        let plugin = LLMMetryPlugin::new(sink.clone())
            .with_model("gpt-4")
            .with_provider("openai");

        let mut step = fix.step(vec![]);

        run_phase(&plugin, Phase::BeforeInference, &step, &fix).await;

        step.extensions.insert(LLMResponse::success(StreamResult {
            text: "hello".into(),
            tool_calls: vec![],
            usage: Some(usage_with_cache(100, 50, 150, 30)),
        }));

        run_phase(&plugin, Phase::AfterInference, &step, &fix).await;

        let m = sink.metrics();
        let span = &m.inferences[0];
        assert_eq!(span.cache_read_input_tokens, Some(30));
        assert!(span.cache_creation_input_tokens.is_none());
    }

    #[tokio::test]
    async fn test_plugin_captures_tool() {
        let fix = TestFixture::new();
        let sink = InMemorySink::new();
        let plugin = LLMMetryPlugin::new(sink.clone());

        let mut step = fix.step(vec![]);

        let call = ToolCall::new("c1", "search", json!({}));
        step.extensions.insert(ToolGate::from_tool_call(&call));

        run_phase(&plugin, Phase::BeforeToolExecute, &step, &fix).await;

        step.extensions.get_mut::<ToolGate>().unwrap().result =
            Some(ToolResult::success("search", json!({"found": true})));

        run_phase(&plugin, Phase::AfterToolExecute, &step, &fix).await;

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
        let fix = TestFixture::new();
        let sink = InMemorySink::new();
        let plugin = LLMMetryPlugin::new(sink.clone());

        let mut step = fix.step(vec![]);

        let call = ToolCall::new("c1", "write", json!({}));
        step.extensions.insert(ToolGate::from_tool_call(&call));

        run_phase(&plugin, Phase::BeforeToolExecute, &step, &fix).await;

        step.extensions.get_mut::<ToolGate>().unwrap().result =
            Some(ToolResult::error("write", "permission denied"));

        run_phase(&plugin, Phase::AfterToolExecute, &step, &fix).await;

        let m = sink.metrics();
        assert!(!m.tools[0].is_success());
        assert_eq!(m.tools[0].error_type.as_deref(), Some("tool_error"));
    }

    #[tokio::test]
    async fn test_plugin_session_lifecycle() {
        let fix = TestFixture::new();
        let sink = InMemorySink::new();
        let plugin = LLMMetryPlugin::new(sink.clone());

        let step = fix.step(vec![]);

        run_phase(&plugin, Phase::RunStart, &step, &fix).await;

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        run_phase(&plugin, Phase::RunEnd, &step, &fix).await;

        let m = sink.metrics();
        assert!(m.session_duration_ms >= 10);
    }

    #[tokio::test]
    async fn test_plugin_no_usage() {
        let fix = TestFixture::new();
        let sink = InMemorySink::new();
        let plugin = LLMMetryPlugin::new(sink.clone()).with_model("m");

        let mut step = fix.step(vec![]);

        run_phase(&plugin, Phase::BeforeInference, &step, &fix).await;
        step.extensions.insert(LLMResponse::success(StreamResult {
            text: "hi".into(),
            tool_calls: vec![],
            usage: None,
        }));
        run_phase(&plugin, Phase::AfterInference, &step, &fix).await;

        let m = sink.metrics();
        assert_eq!(m.inference_count(), 1);
        assert!(m.inferences[0].input_tokens.is_none());
        assert!(m.inferences[0].cache_read_input_tokens.is_none());
    }

    #[tokio::test]
    async fn test_plugin_multiple_rounds() {
        let fix = TestFixture::new();
        let sink = InMemorySink::new();
        let plugin = LLMMetryPlugin::new(sink.clone()).with_model("m");

        let mut step = fix.step(vec![]);

        for i in 0..3 {
            run_phase(&plugin, Phase::BeforeInference, &step, &fix).await;
            step.extensions.insert(LLMResponse::success(StreamResult {
                text: format!("r{i}"),
                tool_calls: vec![],
                usage: Some(usage(10 * (i + 1), 5 * (i + 1), 15 * (i + 1))),
            }));
            run_phase(&plugin, Phase::AfterInference, &step, &fix).await;
        }

        let m = sink.metrics();
        assert_eq!(m.inference_count(), 3);
        assert_eq!(m.total_input_tokens(), 60); // 10+20+30
        assert_eq!(m.total_output_tokens(), 30); // 5+10+15
    }

    #[tokio::test]
    async fn test_plugin_captures_inference_error() {
        let fix = TestFixture::new();
        let sink = InMemorySink::new();
        let plugin = LLMMetryPlugin::new(sink.clone())
            .with_model("gpt-4")
            .with_provider("openai");

        let mut step = fix.step(vec![]);

        run_phase(&plugin, Phase::BeforeInference, &step, &fix).await;
        step.extensions.insert(LLMResponse::error(InferenceError {
            error_type: "rate_limited".to_string(),
            message: "429".to_string(),
        }));
        run_phase(&plugin, Phase::AfterInference, &step, &fix).await;

        let m = sink.metrics();
        assert_eq!(m.inference_count(), 1);
        assert_eq!(m.inferences[0].error_type.as_deref(), Some("rate_limited"));
    }

    #[tokio::test]
    async fn test_plugin_parallel_tool_spans_are_isolated_by_call_id() {
        let fix = TestFixture::new();
        use std::time::Duration;

        let sink = InMemorySink::new();
        let plugin = Arc::new(LLMMetryPlugin::new(sink.clone()).with_provider("p"));

        let calls = vec![
            ToolCall::new("c1", "search", json!({"q": "a"})),
            ToolCall::new("c2", "write", json!({"path": "x"})),
            ToolCall::new("c3", "read", json!({"path": "y"})),
        ];

        let fix = &fix;
        let tasks = calls.into_iter().enumerate().map(|(i, call)| {
            let plugin = plugin.clone();
            async move {
                let mut step = fix.step(vec![]);
                step.extensions.insert(ToolGate::from_tool_call(&call));
                run_phase(&*plugin, Phase::BeforeToolExecute, &step, fix).await;
                // Stagger completion to maximize the chance of cross-talk.
                tokio::time::sleep(Duration::from_millis(5 * (3 - i) as u64)).await;
                step.extensions.get_mut::<ToolGate>().unwrap().result =
                    Some(ToolResult::success(&call.name, json!({"ok": true})));
                run_phase(&*plugin, Phase::AfterToolExecute, &step, fix).await;
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
        let (i, o, t) = extract_token_counts(Some(&u));
        assert_eq!(i, Some(10));
        assert_eq!(o, Some(20));
        assert_eq!(t, Some(30));
    }

    #[test]
    fn test_extract_token_counts_none() {
        let (i, o, t) = extract_token_counts(None);
        assert!(i.is_none());
        assert!(o.is_none());
        assert!(t.is_none());
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
        let fix = TestFixture::new();
        let captured = Arc::new(std::sync::Mutex::new(Vec::<CapturedSpan>::new()));
        let layer = SpanCaptureLayer {
            captured: captured.clone(),
        };
        let subscriber = tracing_subscriber::registry::Registry::default().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        let sink = InMemorySink::new();
        let plugin = LLMMetryPlugin::new(sink.clone())
            .with_model("test-model")
            .with_provider("test-provider");

        let mut step = fix.step(vec![]);

        run_phase(&plugin, Phase::BeforeInference, &step, &fix).await;
        step.extensions.insert(LLMResponse::success(StreamResult {
            text: "hi".into(),
            tool_calls: vec![],
            usage: Some(usage(10, 20, 30)),
        }));
        run_phase(&plugin, Phase::AfterInference, &step, &fix).await;

        let spans = captured.lock().unwrap();
        let inference_span = spans.iter().find(|s| s.name == "gen_ai");
        assert!(inference_span.is_some(), "expected gen_ai span (inference)");
        assert!(inference_span.unwrap().was_closed, "span should be closed");
    }

    #[tokio::test]
    async fn test_tracing_span_tool() {
        let fix = TestFixture::new();
        let captured = Arc::new(std::sync::Mutex::new(Vec::<CapturedSpan>::new()));
        let layer = SpanCaptureLayer {
            captured: captured.clone(),
        };
        let subscriber = tracing_subscriber::registry::Registry::default().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        let sink = InMemorySink::new();
        let plugin = LLMMetryPlugin::new(sink.clone());

        let mut step = fix.step(vec![]);

        let call = ToolCall::new("c1", "search", json!({}));
        step.extensions.insert(ToolGate::from_tool_call(&call));

        run_phase(&plugin, Phase::BeforeToolExecute, &step, &fix).await;
        step.extensions.get_mut::<ToolGate>().unwrap().result =
            Some(ToolResult::success("search", json!({"found": true})));
        run_phase(&plugin, Phase::AfterToolExecute, &step, &fix).await;

        let spans = captured.lock().unwrap();
        let tool_span = spans.iter().find(|s| s.name == "gen_ai");
        assert!(tool_span.is_some(), "expected gen_ai span (tool)");
        assert!(tool_span.unwrap().was_closed, "span should be closed");
    }

    #[test]
    fn test_plugin_id() {
        let sink = InMemorySink::new();
        let plugin = LLMMetryPlugin::new(sink);
        assert_eq!(tirea_contract::AgentBehavior::id(&plugin), "llmmetry");
    }

    // ---- OTel export compatibility tests ----

    mod otel_export {
        use super::*;
        use opentelemetry::trace::TracerProvider as _;
        use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
        use tracing_opentelemetry::OpenTelemetryLayer;
        use tracing_subscriber::layer::SubscriberExt;

        fn setup_otel_test() -> (
            tracing::subscriber::DefaultGuard,
            InMemorySpanExporter,
            SdkTracerProvider,
        ) {
            let exporter = InMemorySpanExporter::default();
            let provider = SdkTracerProvider::builder()
                .with_simple_exporter(exporter.clone())
                .build();
            let tracer = provider.tracer("test");
            let otel_layer = OpenTelemetryLayer::new(tracer);
            let subscriber = tracing_subscriber::registry::Registry::default().with(otel_layer);
            let guard = tracing::subscriber::set_default(subscriber);
            (guard, exporter, provider)
        }

        fn find_attribute<'a>(span: &'a SpanData, key: &str) -> Option<&'a opentelemetry::Value> {
            span.attributes
                .iter()
                .find(|kv| kv.key.as_str() == key)
                .map(|kv| &kv.value)
        }

        #[tokio::test]
        async fn test_otel_export_inference_span() {
            let fix = TestFixture::new();
            let (_guard, exporter, provider) = setup_otel_test();

            let sink = InMemorySink::new();
            let plugin = LLMMetryPlugin::new(sink)
                .with_model("test-model")
                .with_provider("test-provider");

            let mut step = fix.step(vec![]);

            run_phase(&plugin, Phase::BeforeInference, &step, &fix).await;
            step.extensions.insert(LLMResponse::success(StreamResult {
                text: "hello".into(),
                tool_calls: vec![],
                usage: Some(usage(100, 50, 150)),
            }));
            run_phase(&plugin, Phase::AfterInference, &step, &fix).await;

            let _ = provider.force_flush();
            let exported = exporter.get_finished_spans().unwrap();
            let span = exported
                .iter()
                .find(|s| s.name.starts_with("chat "))
                .expect("expected chat span in OTel export");

            assert_eq!(
                find_attribute(span, "gen_ai.provider.name")
                    .unwrap()
                    .as_str(),
                "test-provider"
            );
            assert_eq!(
                find_attribute(span, "gen_ai.operation.name")
                    .unwrap()
                    .as_str(),
                "chat"
            );
            assert_eq!(
                find_attribute(span, "gen_ai.request.model")
                    .unwrap()
                    .as_str(),
                "test-model"
            );
            assert_eq!(
                find_attribute(span, "gen_ai.usage.input_tokens"),
                Some(&opentelemetry::Value::I64(100))
            );
            assert_eq!(
                find_attribute(span, "gen_ai.usage.output_tokens"),
                Some(&opentelemetry::Value::I64(50))
            );
            // Duration is captured by OTel span timestamps, not as a custom attribute
            assert!(
                find_attribute(span, "gen_ai.client.operation.duration_ms").is_none(),
                "duration should not be a custom attribute (OTel captures it via span timestamps)"
            );
        }

        #[tokio::test]
        async fn test_otel_export_inference_error_sets_status_and_error_type() {
            let fix = TestFixture::new();
            use opentelemetry::trace::Status;

            let (_guard, exporter, provider) = setup_otel_test();

            let sink = InMemorySink::new();
            let plugin = LLMMetryPlugin::new(sink)
                .with_model("test-model")
                .with_provider("test-provider");

            let mut step = fix.step(vec![]);

            run_phase(&plugin, Phase::BeforeInference, &step, &fix).await;
            step.extensions.insert(LLMResponse::error(InferenceError {
                error_type: "rate_limited".to_string(),
                message: "429".to_string(),
            }));
            run_phase(&plugin, Phase::AfterInference, &step, &fix).await;

            let _ = provider.force_flush();
            let exported = exporter.get_finished_spans().unwrap();
            let span = exported
                .iter()
                .find(|s| s.name.starts_with("chat "))
                .expect("expected chat span in OTel export");

            assert_eq!(
                find_attribute(span, "error.type").unwrap().as_str(),
                "rate_limited"
            );
            assert!(
                matches!(span.status, Status::Error { .. }),
                "failed inference span should have OTel Status::Error, got {:?}",
                span.status
            );
        }

        #[tokio::test]
        async fn test_otel_export_parent_child_propagation() {
            let fix = TestFixture::new();
            let (_guard, exporter, provider) = setup_otel_test();

            let sink = InMemorySink::new();
            let plugin = LLMMetryPlugin::new(sink)
                .with_model("test-model")
                .with_provider("test-provider");

            // Create and enter a parent span
            let parent = tracing::info_span!("parent_operation");
            let _parent_guard = parent.enter();

            let mut step = fix.step(vec![]);

            run_phase(&plugin, Phase::BeforeInference, &step, &fix).await;

            step.extensions.insert(LLMResponse::success(StreamResult {
                text: "hello".into(),
                tool_calls: vec![],
                usage: Some(usage(100, 50, 150)),
            }));
            run_phase(&plugin, Phase::AfterInference, &step, &fix).await;

            drop(_parent_guard);
            drop(parent);

            let _ = provider.force_flush();
            let exported = exporter.get_finished_spans().unwrap();

            let parent_span = exported
                .iter()
                .find(|s| s.name == "parent_operation")
                .expect("expected parent_operation span");
            let child_span = exported
                .iter()
                .find(|s| s.name.starts_with("chat "))
                .expect("expected chat span");

            // The child span should share the same trace_id as the parent
            assert_eq!(
                child_span.span_context.trace_id(),
                parent_span.span_context.trace_id(),
                "chat span should share parent's trace_id"
            );
            // The child span's parent_span_id should be the parent's span_id
            assert_eq!(
                child_span.parent_span_id,
                parent_span.span_context.span_id(),
                "chat span should have parent's span_id as parent_span_id"
            );
        }

        #[tokio::test]
        async fn test_otel_export_tool_parent_child_propagation() {
            let fix = TestFixture::new();
            let (_guard, exporter, provider) = setup_otel_test();

            let sink = InMemorySink::new();
            let plugin = LLMMetryPlugin::new(sink);

            let parent = tracing::info_span!("parent_tool_op");
            let _parent_guard = parent.enter();

            let mut step = fix.step(vec![]);

            let call = ToolCall::new("tc1", "search", json!({}));
            step.extensions.insert(ToolGate::from_tool_call(&call));

            run_phase(&plugin, Phase::BeforeToolExecute, &step, &fix).await;

            step.extensions.get_mut::<ToolGate>().unwrap().result =
                Some(ToolResult::success("search", json!({"found": true})));
            run_phase(&plugin, Phase::AfterToolExecute, &step, &fix).await;

            drop(_parent_guard);
            drop(parent);

            let _ = provider.force_flush();
            let exported = exporter.get_finished_spans().unwrap();

            let parent_span = exported
                .iter()
                .find(|s| s.name == "parent_tool_op")
                .expect("expected parent_tool_op span");
            let child_span = exported
                .iter()
                .find(|s| s.name.starts_with("execute_tool "))
                .expect("expected execute_tool span");

            assert_eq!(
                child_span.span_context.trace_id(),
                parent_span.span_context.trace_id(),
                "execute_tool span should share parent's trace_id"
            );
            assert_eq!(
                child_span.parent_span_id,
                parent_span.span_context.span_id(),
                "execute_tool span should have parent's span_id as parent_span_id"
            );
        }

        #[tokio::test]
        async fn test_otel_export_no_parent_context() {
            let fix = TestFixture::new();
            let (_guard, exporter, provider) = setup_otel_test();

            let sink = InMemorySink::new();
            let plugin = LLMMetryPlugin::new(sink).with_model("m").with_provider("p");

            // No parent span entered — should be a root span
            let mut step = fix.step(vec![]);

            run_phase(&plugin, Phase::BeforeInference, &step, &fix).await;
            step.extensions.insert(LLMResponse::success(StreamResult {
                text: "hi".into(),
                tool_calls: vec![],
                usage: None,
            }));
            run_phase(&plugin, Phase::AfterInference, &step, &fix).await;

            let _ = provider.force_flush();
            let exported = exporter.get_finished_spans().unwrap();

            let span = exported
                .iter()
                .find(|s| s.name.starts_with("chat "))
                .expect("expected chat span");

            // Root span should have invalid parent_span_id
            assert_eq!(
                span.parent_span_id,
                opentelemetry::trace::SpanId::INVALID,
                "root span should have no parent"
            );
        }

        #[tokio::test]
        async fn test_otel_export_spans_closed_after_phases() {
            let fix = TestFixture::new();
            let (_guard, exporter, provider) = setup_otel_test();

            let sink = InMemorySink::new();
            let plugin = LLMMetryPlugin::new(sink).with_model("m").with_provider("p");

            // Inference span should be closed (exported) after AfterInference
            {
                let mut step = fix.step(vec![]);
                run_phase(&plugin, Phase::BeforeInference, &step, &fix).await;
                step.extensions.insert(LLMResponse::success(StreamResult {
                    text: "hi".into(),
                    tool_calls: vec![],
                    usage: None,
                }));
                run_phase(&plugin, Phase::AfterInference, &step, &fix).await;
            }

            // Tool span should be closed (exported) after AfterToolExecute
            {
                let mut step = fix.step(vec![]);
                let call = ToolCall::new("c1", "test", json!({}));
                step.extensions.insert(ToolGate::from_tool_call(&call));
                run_phase(&plugin, Phase::BeforeToolExecute, &step, &fix).await;
                step.extensions.get_mut::<ToolGate>().unwrap().result =
                    Some(ToolResult::success("test", json!({})));
                run_phase(&plugin, Phase::AfterToolExecute, &step, &fix).await;
            }

            let _ = provider.force_flush();
            let exported = exporter.get_finished_spans().unwrap();
            assert!(
                exported.iter().any(|s| s.name.starts_with("chat ")),
                "inference span should be exported (closed) after AfterInference"
            );
            assert!(
                exported.iter().any(|s| s.name.starts_with("execute_tool ")),
                "tool span should be exported (closed) after AfterToolExecute"
            );
        }

        #[tokio::test]
        async fn test_otel_export_inference_and_tool_are_siblings() {
            let fix = TestFixture::new();
            let (_guard, exporter, provider) = setup_otel_test();

            let sink = InMemorySink::new();
            let plugin = LLMMetryPlugin::new(sink).with_model("m").with_provider("p");

            let parent = tracing::info_span!("agent_step");
            let _parent_guard = parent.enter();

            // Inference phase
            {
                let mut step = fix.step(vec![]);
                run_phase(&plugin, Phase::BeforeInference, &step, &fix).await;
                step.extensions.insert(LLMResponse::success(StreamResult {
                    text: "calling tool".into(),
                    tool_calls: vec![ToolCall::new("c1", "search", json!({}))],
                    usage: Some(usage(10, 5, 15)),
                }));
                run_phase(&plugin, Phase::AfterInference, &step, &fix).await;
            }

            // Tool phase
            {
                let mut step = fix.step(vec![]);
                let call = ToolCall::new("c1", "search", json!({}));
                step.extensions.insert(ToolGate::from_tool_call(&call));
                run_phase(&plugin, Phase::BeforeToolExecute, &step, &fix).await;
                step.extensions.get_mut::<ToolGate>().unwrap().result =
                    Some(ToolResult::success("search", json!({})));
                run_phase(&plugin, Phase::AfterToolExecute, &step, &fix).await;
            }

            drop(_parent_guard);
            drop(parent);

            let _ = provider.force_flush();
            let exported = exporter.get_finished_spans().unwrap();

            let parent_span = exported
                .iter()
                .find(|s| s.name == "agent_step")
                .expect("expected agent_step span");
            let inference_span = exported
                .iter()
                .find(|s| s.name.starts_with("chat "))
                .expect("expected chat span");
            let tool_span = exported
                .iter()
                .find(|s| s.name.starts_with("execute_tool "))
                .expect("expected execute_tool span");

            // Both should share the same trace
            assert_eq!(
                inference_span.span_context.trace_id(),
                parent_span.span_context.trace_id(),
            );
            assert_eq!(
                tool_span.span_context.trace_id(),
                parent_span.span_context.trace_id(),
            );

            // Both should be children of the parent (siblings, not nested)
            assert_eq!(
                inference_span.parent_span_id,
                parent_span.span_context.span_id(),
                "inference span should be child of agent_step"
            );
            assert_eq!(
                tool_span.parent_span_id,
                parent_span.span_context.span_id(),
                "tool span should be child of agent_step"
            );

            // They should be distinct spans
            assert_ne!(
                inference_span.span_context.span_id(),
                tool_span.span_context.span_id(),
                "inference and tool should be distinct sibling spans"
            );
        }

        #[tokio::test]
        async fn test_otel_export_tool_span() {
            let fix = TestFixture::new();
            let (_guard, exporter, provider) = setup_otel_test();

            let sink = InMemorySink::new();
            let plugin = LLMMetryPlugin::new(sink);

            let mut step = fix.step(vec![]);

            let call = ToolCall::new("tc1", "search", json!({}));
            step.extensions.insert(ToolGate::from_tool_call(&call));

            run_phase(&plugin, Phase::BeforeToolExecute, &step, &fix).await;
            step.extensions.get_mut::<ToolGate>().unwrap().result =
                Some(ToolResult::success("search", json!({"found": true})));
            run_phase(&plugin, Phase::AfterToolExecute, &step, &fix).await;

            let _ = provider.force_flush();
            let exported = exporter.get_finished_spans().unwrap();
            let span = exported
                .iter()
                .find(|s| s.name.starts_with("execute_tool "))
                .expect("expected execute_tool span in OTel export");

            assert_eq!(
                find_attribute(span, "gen_ai.tool.name").unwrap().as_str(),
                "search"
            );
            assert_eq!(
                find_attribute(span, "gen_ai.tool.call.id")
                    .unwrap()
                    .as_str(),
                "tc1"
            );
            assert_eq!(
                find_attribute(span, "gen_ai.operation.name")
                    .unwrap()
                    .as_str(),
                "execute_tool"
            );
        }

        // ==================================================================
        // OTel GenAI Semantic Conventions compliance tests
        // ==================================================================

        #[tokio::test]
        async fn test_otel_semconv_inference_span_name_format() {
            let fix = TestFixture::new();
            let (_guard, exporter, provider) = setup_otel_test();

            let sink = InMemorySink::new();
            let plugin = LLMMetryPlugin::new(sink)
                .with_model("gpt-4o")
                .with_provider("openai");

            let mut step = fix.step(vec![]);

            run_phase(&plugin, Phase::BeforeInference, &step, &fix).await;
            step.extensions.insert(LLMResponse::success(StreamResult {
                text: "hi".into(),
                tool_calls: vec![],
                usage: None,
            }));
            run_phase(&plugin, Phase::AfterInference, &step, &fix).await;

            let _ = provider.force_flush();
            let exported = exporter.get_finished_spans().unwrap();
            let span = exported
                .iter()
                .find(|s| s.name.starts_with("chat "))
                .expect("expected chat span");

            // Spec: span name = "{gen_ai.operation.name} {gen_ai.request.model}"
            assert_eq!(
                span.name.as_ref(),
                "chat gpt-4o",
                "span name should follow OTel GenAI format: '{{operation}} {{model}}'"
            );
        }

        #[tokio::test]
        async fn test_otel_semconv_tool_span_name_format() {
            let fix = TestFixture::new();
            let (_guard, exporter, provider) = setup_otel_test();

            let sink = InMemorySink::new();
            let plugin = LLMMetryPlugin::new(sink).with_provider("openai");

            let mut step = fix.step(vec![]);

            let call = ToolCall::new("tc1", "web_search", json!({}));
            step.extensions.insert(ToolGate::from_tool_call(&call));

            run_phase(&plugin, Phase::BeforeToolExecute, &step, &fix).await;
            step.extensions.get_mut::<ToolGate>().unwrap().result =
                Some(ToolResult::success("web_search", json!({})));
            run_phase(&plugin, Phase::AfterToolExecute, &step, &fix).await;

            let _ = provider.force_flush();
            let exported = exporter.get_finished_spans().unwrap();
            let span = exported
                .iter()
                .find(|s| s.name.starts_with("execute_tool "))
                .expect("expected execute_tool span");

            // Spec: span name = "execute_tool {gen_ai.tool.name}"
            assert_eq!(
                span.name.as_ref(),
                "execute_tool web_search",
                "span name should follow OTel GenAI format: 'execute_tool {{tool_name}}'"
            );
        }

        #[tokio::test]
        async fn test_otel_semconv_inference_span_kind_client() {
            let fix = TestFixture::new();
            use opentelemetry::trace::SpanKind;

            let (_guard, exporter, provider) = setup_otel_test();

            let sink = InMemorySink::new();
            let plugin = LLMMetryPlugin::new(sink).with_model("m").with_provider("p");

            let mut step = fix.step(vec![]);

            run_phase(&plugin, Phase::BeforeInference, &step, &fix).await;
            step.extensions.insert(LLMResponse::success(StreamResult {
                text: "hi".into(),
                tool_calls: vec![],
                usage: None,
            }));
            run_phase(&plugin, Phase::AfterInference, &step, &fix).await;

            let _ = provider.force_flush();
            let exported = exporter.get_finished_spans().unwrap();
            let span = exported
                .iter()
                .find(|s| s.name.starts_with("chat "))
                .expect("expected chat span");

            // Spec: chat spans should be SpanKind::Client
            assert_eq!(
                span.span_kind,
                SpanKind::Client,
                "inference span should have SpanKind::Client per OTel GenAI spec"
            );
        }

        #[tokio::test]
        async fn test_otel_semconv_tool_span_kind_internal() {
            let fix = TestFixture::new();
            use opentelemetry::trace::SpanKind;

            let (_guard, exporter, provider) = setup_otel_test();

            let sink = InMemorySink::new();
            let plugin = LLMMetryPlugin::new(sink).with_provider("p");

            let mut step = fix.step(vec![]);

            let call = ToolCall::new("tc1", "search", json!({}));
            step.extensions.insert(ToolGate::from_tool_call(&call));

            run_phase(&plugin, Phase::BeforeToolExecute, &step, &fix).await;
            step.extensions.get_mut::<ToolGate>().unwrap().result =
                Some(ToolResult::success("search", json!({})));
            run_phase(&plugin, Phase::AfterToolExecute, &step, &fix).await;

            let _ = provider.force_flush();
            let exported = exporter.get_finished_spans().unwrap();
            let span = exported
                .iter()
                .find(|s| s.name.starts_with("execute_tool "))
                .expect("expected execute_tool span");

            // Spec: tool spans should be SpanKind::Internal
            assert_eq!(
                span.span_kind,
                SpanKind::Internal,
                "tool span should have SpanKind::Internal per OTel GenAI spec"
            );
        }

        #[tokio::test]
        async fn test_otel_semconv_tool_span_has_provider() {
            let fix = TestFixture::new();
            let (_guard, exporter, provider) = setup_otel_test();

            let sink = InMemorySink::new();
            let plugin = LLMMetryPlugin::new(sink).with_provider("anthropic");

            let mut step = fix.step(vec![]);

            let call = ToolCall::new("tc1", "search", json!({}));
            step.extensions.insert(ToolGate::from_tool_call(&call));

            run_phase(&plugin, Phase::BeforeToolExecute, &step, &fix).await;
            step.extensions.get_mut::<ToolGate>().unwrap().result =
                Some(ToolResult::success("search", json!({})));
            run_phase(&plugin, Phase::AfterToolExecute, &step, &fix).await;

            let _ = provider.force_flush();
            let exported = exporter.get_finished_spans().unwrap();
            let span = exported
                .iter()
                .find(|s| s.name.starts_with("execute_tool "))
                .expect("expected execute_tool span");

            // Spec: gen_ai.provider.name is required on all GenAI spans
            assert_eq!(
                find_attribute(span, "gen_ai.provider.name")
                    .unwrap()
                    .as_str(),
                "anthropic",
                "tool span must include gen_ai.provider.name per OTel GenAI spec"
            );
        }

        #[tokio::test]
        async fn test_otel_semconv_error_sets_status_code() {
            let fix = TestFixture::new();
            use opentelemetry::trace::Status;

            let (_guard, exporter, provider) = setup_otel_test();

            let sink = InMemorySink::new();
            let plugin = LLMMetryPlugin::new(sink).with_provider("p");

            let mut step = fix.step(vec![]);

            let call = ToolCall::new("tc1", "write", json!({}));
            step.extensions.insert(ToolGate::from_tool_call(&call));

            run_phase(&plugin, Phase::BeforeToolExecute, &step, &fix).await;
            step.extensions.get_mut::<ToolGate>().unwrap().result =
                Some(ToolResult::error("write", "permission denied"));
            run_phase(&plugin, Phase::AfterToolExecute, &step, &fix).await;

            let _ = provider.force_flush();
            let exported = exporter.get_finished_spans().unwrap();
            let span = exported
                .iter()
                .find(|s| s.name.starts_with("execute_tool "))
                .expect("expected execute_tool span");

            // Spec: error.type should be set
            assert_eq!(
                find_attribute(span, "error.type").unwrap().as_str(),
                "tool_error"
            );

            // Spec: OTel status should be Error on failure
            assert!(
                matches!(span.status, Status::Error { .. }),
                "failed tool span should have OTel Status::Error, got {:?}",
                span.status
            );
        }

        #[tokio::test]
        async fn test_otel_semconv_success_no_error_status() {
            let fix = TestFixture::new();
            use opentelemetry::trace::Status;

            let (_guard, exporter, provider) = setup_otel_test();

            let sink = InMemorySink::new();
            let plugin = LLMMetryPlugin::new(sink).with_model("m").with_provider("p");

            let mut step = fix.step(vec![]);

            run_phase(&plugin, Phase::BeforeInference, &step, &fix).await;
            step.extensions.insert(LLMResponse::success(StreamResult {
                text: "ok".into(),
                tool_calls: vec![],
                usage: Some(usage(10, 5, 15)),
            }));
            run_phase(&plugin, Phase::AfterInference, &step, &fix).await;

            let _ = provider.force_flush();
            let exported = exporter.get_finished_spans().unwrap();
            let span = exported
                .iter()
                .find(|s| s.name.starts_with("chat "))
                .expect("expected chat span");

            // Spec: successful spans should NOT have Error status
            assert!(
                !matches!(span.status, Status::Error { .. }),
                "successful span should not have Error status"
            );
            assert!(
                find_attribute(span, "error.type").is_none(),
                "successful span should not have error.type attribute"
            );
        }

        #[tokio::test]
        async fn test_otel_semconv_required_attributes_present() {
            let fix = TestFixture::new();
            let (_guard, exporter, provider) = setup_otel_test();

            let sink = InMemorySink::new();
            let plugin = LLMMetryPlugin::new(sink)
                .with_model("claude-3")
                .with_provider("anthropic");

            let mut step = fix.step(vec![]);

            run_phase(&plugin, Phase::BeforeInference, &step, &fix).await;
            step.extensions.insert(LLMResponse::success(StreamResult {
                text: "hi".into(),
                tool_calls: vec![],
                usage: Some(usage(100, 50, 150)),
            }));
            run_phase(&plugin, Phase::AfterInference, &step, &fix).await;

            let _ = provider.force_flush();
            let exported = exporter.get_finished_spans().unwrap();
            let span = exported
                .iter()
                .find(|s| s.name.starts_with("chat "))
                .expect("expected chat span");

            // Required: gen_ai.operation.name
            assert_eq!(
                find_attribute(span, "gen_ai.operation.name")
                    .unwrap()
                    .as_str(),
                "chat"
            );
            // Required: gen_ai.provider.name
            assert_eq!(
                find_attribute(span, "gen_ai.provider.name")
                    .unwrap()
                    .as_str(),
                "anthropic"
            );
            // Conditionally required: gen_ai.request.model
            assert_eq!(
                find_attribute(span, "gen_ai.request.model")
                    .unwrap()
                    .as_str(),
                "claude-3"
            );
            // Recommended: gen_ai.usage.input_tokens
            assert_eq!(
                find_attribute(span, "gen_ai.usage.input_tokens"),
                Some(&opentelemetry::Value::I64(100))
            );
            // Recommended: gen_ai.usage.output_tokens
            assert_eq!(
                find_attribute(span, "gen_ai.usage.output_tokens"),
                Some(&opentelemetry::Value::I64(50))
            );
        }

        #[tokio::test]
        async fn test_otel_semconv_no_usage_omits_token_attributes() {
            let fix = TestFixture::new();
            let (_guard, exporter, provider) = setup_otel_test();

            let sink = InMemorySink::new();
            let plugin = LLMMetryPlugin::new(sink).with_model("m").with_provider("p");

            let mut step = fix.step(vec![]);

            run_phase(&plugin, Phase::BeforeInference, &step, &fix).await;
            step.extensions.insert(LLMResponse::success(StreamResult {
                text: "hi".into(),
                tool_calls: vec![],
                usage: None,
            }));
            run_phase(&plugin, Phase::AfterInference, &step, &fix).await;

            let _ = provider.force_flush();
            let exported = exporter.get_finished_spans().unwrap();
            let span = exported
                .iter()
                .find(|s| s.name.starts_with("chat "))
                .expect("expected chat span");

            // When usage is None, token attributes should not be recorded
            // (Empty fields are not exported as OTel attributes)
            assert!(
                find_attribute(span, "gen_ai.usage.input_tokens").is_none(),
                "input_tokens should be absent when usage is unavailable"
            );
            assert!(
                find_attribute(span, "gen_ai.usage.output_tokens").is_none(),
                "output_tokens should be absent when usage is unavailable"
            );
        }

        #[tokio::test]
        async fn test_otel_semconv_cache_tokens_only_when_present() {
            let fix = TestFixture::new();
            let (_guard, exporter, provider) = setup_otel_test();

            let sink = InMemorySink::new();
            let plugin = LLMMetryPlugin::new(sink).with_model("m").with_provider("p");

            // Test with cache tokens present
            {
                let mut step = fix.step(vec![]);
                run_phase(&plugin, Phase::BeforeInference, &step, &fix).await;
                step.extensions.insert(LLMResponse::success(StreamResult {
                    text: "hi".into(),
                    tool_calls: vec![],
                    usage: Some(usage_with_cache(100, 50, 150, 30)),
                }));
                run_phase(&plugin, Phase::AfterInference, &step, &fix).await;
            }

            let _ = provider.force_flush();
            let exported = exporter.get_finished_spans().unwrap();
            let span = exported
                .iter()
                .find(|s| s.name.starts_with("chat "))
                .expect("expected chat span");

            assert_eq!(
                find_attribute(span, "gen_ai.usage.cache_read.input_tokens"),
                Some(&opentelemetry::Value::I64(30)),
                "cache_read tokens should be recorded when present"
            );
            // cache_creation not set in test fixture
            assert!(
                find_attribute(span, "gen_ai.usage.cache_creation.input_tokens").is_none(),
                "cache_creation tokens should be absent when not provided"
            );
        }

        // ==================================================================
        // Request parameter + tool type tests
        // ==================================================================

        #[tokio::test]
        async fn test_plugin_captures_request_params() {
            let fix = TestFixture::new();
            let sink = InMemorySink::new();

            let opts = genai::chat::ChatOptions::default()
                .with_temperature(0.7)
                .with_max_tokens(2048)
                .with_top_p(0.9);

            let plugin = LLMMetryPlugin::new(sink.clone())
                .with_model("m")
                .with_provider("p")
                .with_chat_options(&opts);

            let mut step = fix.step(vec![]);

            run_phase(&plugin, Phase::BeforeInference, &step, &fix).await;
            step.extensions.insert(LLMResponse::success(StreamResult {
                text: "hi".into(),
                tool_calls: vec![],
                usage: Some(usage(10, 5, 15)),
            }));
            run_phase(&plugin, Phase::AfterInference, &step, &fix).await;

            let m = sink.metrics();
            let span = &m.inferences[0];
            assert_eq!(span.temperature, Some(0.7));
            assert_eq!(span.top_p, Some(0.9));
            assert_eq!(span.max_tokens, Some(2048));
            assert!(span.stop_sequences.is_empty());
        }

        #[tokio::test]
        async fn test_otel_export_request_params() {
            let fix = TestFixture::new();
            let (_guard, exporter, provider) = setup_otel_test();

            let opts = genai::chat::ChatOptions::default()
                .with_temperature(0.5)
                .with_max_tokens(1024);

            let sink = InMemorySink::new();
            let plugin = LLMMetryPlugin::new(sink)
                .with_model("m")
                .with_provider("p")
                .with_chat_options(&opts);

            let mut step = fix.step(vec![]);

            run_phase(&plugin, Phase::BeforeInference, &step, &fix).await;
            step.extensions.insert(LLMResponse::success(StreamResult {
                text: "hi".into(),
                tool_calls: vec![],
                usage: None,
            }));
            run_phase(&plugin, Phase::AfterInference, &step, &fix).await;

            let _ = provider.force_flush();
            let exported = exporter.get_finished_spans().unwrap();
            let span = exported
                .iter()
                .find(|s| s.name.starts_with("chat "))
                .expect("expected chat span");

            assert_eq!(
                find_attribute(span, "gen_ai.request.temperature"),
                Some(&opentelemetry::Value::F64(0.5)),
            );
            assert_eq!(
                find_attribute(span, "gen_ai.request.max_tokens"),
                Some(&opentelemetry::Value::I64(1024)),
            );
            // top_p not set → should be absent
            assert!(
                find_attribute(span, "gen_ai.request.top_p").is_none(),
                "top_p should be absent when not configured"
            );
        }

        #[tokio::test]
        async fn test_otel_export_request_params_omitted_when_none() {
            let fix = TestFixture::new();
            let (_guard, exporter, provider) = setup_otel_test();

            let sink = InMemorySink::new();
            let plugin = LLMMetryPlugin::new(sink).with_model("m").with_provider("p");
            // No with_chat_options → all request params are None

            let mut step = fix.step(vec![]);

            run_phase(&plugin, Phase::BeforeInference, &step, &fix).await;
            step.extensions.insert(LLMResponse::success(StreamResult {
                text: "hi".into(),
                tool_calls: vec![],
                usage: None,
            }));
            run_phase(&plugin, Phase::AfterInference, &step, &fix).await;

            let _ = provider.force_flush();
            let exported = exporter.get_finished_spans().unwrap();
            let span = exported
                .iter()
                .find(|s| s.name.starts_with("chat "))
                .expect("expected chat span");

            assert!(find_attribute(span, "gen_ai.request.temperature").is_none());
            assert!(find_attribute(span, "gen_ai.request.top_p").is_none());
            assert!(find_attribute(span, "gen_ai.request.max_tokens").is_none());
            assert!(find_attribute(span, "gen_ai.request.stop_sequences").is_none());
        }

        #[tokio::test]
        async fn test_otel_export_tool_type_function() {
            let fix = TestFixture::new();
            let (_guard, exporter, provider) = setup_otel_test();

            let sink = InMemorySink::new();
            let plugin = LLMMetryPlugin::new(sink).with_provider("p");

            let mut step = fix.step(vec![]);

            let call = ToolCall::new("tc1", "search", json!({}));
            step.extensions.insert(ToolGate::from_tool_call(&call));

            run_phase(&plugin, Phase::BeforeToolExecute, &step, &fix).await;
            step.extensions.get_mut::<ToolGate>().unwrap().result =
                Some(ToolResult::success("search", json!({})));
            run_phase(&plugin, Phase::AfterToolExecute, &step, &fix).await;

            let _ = provider.force_flush();
            let exported = exporter.get_finished_spans().unwrap();
            let span = exported
                .iter()
                .find(|s| s.name.starts_with("execute_tool "))
                .expect("expected execute_tool span");

            assert_eq!(
                find_attribute(span, "gen_ai.tool.type").unwrap().as_str(),
                "function",
                "tool span must include gen_ai.tool.type = 'function' per OTel GenAI spec"
            );
        }
    }
}
