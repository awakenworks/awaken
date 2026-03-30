use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;

use awaken_contract::StateError;
use awaken_contract::model::Phase;
use awaken_runtime::{Plugin, PluginDescriptor, PluginRegistrar};
use tokio::sync::Mutex;

use crate::metrics::{AgentMetrics, SpanContext};
use crate::sink::MetricsSink;

use super::hooks::{
    AfterInferenceHook, AfterToolExecuteHook, BeforeInferenceHook, BeforeToolExecuteHook,
    RunEndHook, RunStartHook,
};
use super::shared::Inner;

/// Plugin that captures LLM and tool telemetry aligned with OpenTelemetry GenAI conventions.
pub struct ObservabilityPlugin {
    pub(crate) inner: Arc<Inner>,
}

impl ObservabilityPlugin {
    pub fn new(sink: impl MetricsSink + 'static) -> Self {
        Self {
            inner: Arc::new(Inner {
                sink: Arc::new(sink),
                run_start: Mutex::new(None),
                metrics: Mutex::new(AgentMetrics::default()),
                inference_start: Mutex::new(None),
                tool_start: Mutex::new(HashMap::new()),
                model: Mutex::new(String::new()),
                provider: Mutex::new(String::new()),
                operation: "chat".to_string(),
                temperature: Mutex::new(None),
                top_p: Mutex::new(None),
                max_tokens: Mutex::new(None),
                stop_sequences: Mutex::new(Vec::new()),
                inference_tracing_span: Mutex::new(None),
                tool_tracing_span: Mutex::new(HashMap::new()),
                span_context: Mutex::new(SpanContext::default()),
                step_counter: AtomicU32::new(0),
            }),
        }
    }

    #[must_use]
    pub fn with_model(self, model: impl Into<String>) -> Self {
        *self
            .inner
            .model
            .try_lock()
            .expect("no contention during builder") = model.into();
        self
    }

    #[must_use]
    pub fn with_provider(self, provider: impl Into<String>) -> Self {
        *self
            .inner
            .provider
            .try_lock()
            .expect("no contention during builder") = provider.into();
        self
    }

    #[must_use]
    pub fn with_temperature(self, temperature: f64) -> Self {
        *self
            .inner
            .temperature
            .try_lock()
            .expect("no contention during builder") = Some(temperature);
        self
    }

    #[must_use]
    pub fn with_top_p(self, top_p: f64) -> Self {
        *self
            .inner
            .top_p
            .try_lock()
            .expect("no contention during builder") = Some(top_p);
        self
    }

    #[must_use]
    pub fn with_max_tokens(self, max_tokens: u32) -> Self {
        *self
            .inner
            .max_tokens
            .try_lock()
            .expect("no contention during builder") = Some(max_tokens);
        self
    }

    #[must_use]
    pub fn with_stop_sequences(self, seqs: Vec<String>) -> Self {
        *self
            .inner
            .stop_sequences
            .try_lock()
            .expect("no contention during builder") = seqs;
        self
    }
}

/// Stable plugin ID for the observability extension.
pub const OBSERVABILITY_PLUGIN_ID: &str = "observability";

impl Plugin for ObservabilityPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: OBSERVABILITY_PLUGIN_ID,
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        let id = OBSERVABILITY_PLUGIN_ID;
        let s = Arc::clone(&self.inner);
        registrar.register_phase_hook(id, Phase::RunStart, RunStartHook(Arc::clone(&s)))?;
        registrar.register_phase_hook(
            id,
            Phase::BeforeInference,
            BeforeInferenceHook(Arc::clone(&s)),
        )?;
        registrar.register_phase_hook(
            id,
            Phase::AfterInference,
            AfterInferenceHook(Arc::clone(&s)),
        )?;
        registrar.register_phase_hook(
            id,
            Phase::BeforeToolExecute,
            BeforeToolExecuteHook(Arc::clone(&s)),
        )?;
        registrar.register_phase_hook(
            id,
            Phase::AfterToolExecute,
            AfterToolExecuteHook(Arc::clone(&s)),
        )?;
        registrar.register_phase_hook(id, Phase::RunEnd, RunEndHook(Arc::clone(&s)))?;
        Ok(())
    }
}
