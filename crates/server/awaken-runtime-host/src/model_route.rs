//! Per-thread model→executor routing (R1/R2).
//!
//! The host once held a single fixed `LlmExecutor`; this module lifts that to a
//! per-thread binding so a session (and, with a per-turn override, a turn) selects
//! its own model. [`ThreadModelBinding`] owns the two moving parts — an optional
//! [`ExecutorProvider`] and the per-thread model refs staged at session prepare —
//! and resolves a thread's executor, falling back to the host default when no
//! provider is installed or a ref does not resolve (so a single-model deployment
//! behaves exactly as before).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use awaken_runtime_contract::llm::LlmExecutor;

/// Resolves a bound model ref to its executor (R1). `None` from `executor_for`
/// means "use the host default" — a deployment with one fixed model registers no
/// provider and is unaffected.
pub trait ExecutorProvider: Send + Sync {
    /// The executor for `model_ref`, or `None` to fall back to the host default.
    fn executor_for(&self, model_ref: &str) -> Option<Arc<dyn LlmExecutor>>;
}

/// The host's per-thread model binding: which model ref each thread runs, and how
/// a ref becomes an executor.
pub(crate) struct ThreadModelBinding {
    provider: Option<Arc<dyn ExecutorProvider>>,
    /// Per-thread bound model ref, staged at session prepare (mirrors `thread_mcp`).
    /// Absent → the host default model ref.
    thread_model: Mutex<HashMap<String, String>>,
}

impl ThreadModelBinding {
    pub(crate) fn new() -> Self {
        Self {
            provider: None,
            thread_model: Mutex::new(HashMap::new()),
        }
    }

    /// Install the resolver (R1). Without one, every thread uses the host default.
    pub(crate) fn set_provider(&mut self, provider: Arc<dyn ExecutorProvider>) {
        self.provider = Some(provider);
    }

    /// The installed model→executor provider, if any — for wiring the neutral
    /// resolver closure a durable worker's context carries.
    pub(crate) fn provider(&self) -> Option<Arc<dyn ExecutorProvider>> {
        self.provider.clone()
    }

    /// Bind `model_ref` to `thread` (R2/R5), staged before its first turn.
    /// Re-registering replaces the binding (the per-turn override re-stages).
    pub(crate) fn register(&self, thread: &str, model_ref: impl Into<String>) {
        self.thread_model
            .lock()
            .expect("thread model mutex poisoned")
            .insert(thread.to_string(), model_ref.into());
    }

    /// The model ref bound to `thread`, or `default_ref`.
    pub(crate) fn model_ref(&self, thread: &str, default_ref: &str) -> String {
        self.thread_model
            .lock()
            .expect("thread model mutex poisoned")
            .get(thread)
            .cloned()
            .unwrap_or_else(|| default_ref.to_string())
    }

    /// The per-thread model override (R2/R5), if one was staged — `None` when the run
    /// uses its agent's published binding. Stamped onto the run's activation at
    /// delivery so the resolve seam sees it without consulting this map.
    pub(crate) fn override_for(&self, thread: &str) -> Option<String> {
        self.thread_model
            .lock()
            .expect("thread model mutex poisoned")
            .get(thread)
            .cloned()
    }

    /// Resolve an effective model ref to its executor through the installed provider
    /// (R1), or `None` to fall back to the runtime's bound (host default) executor.
    /// This is the per-run executor seam: a run names its effective model and gets an
    /// executor, resolved each attempt from the run's own binding rather than a
    /// session-build-time registry lookup.
    pub(crate) fn executor_for(&self, model_ref: &str) -> Option<Arc<dyn LlmExecutor>> {
        self.provider
            .as_ref()
            .and_then(|provider| provider.executor_for(model_ref))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse};

    struct LabeledModel(&'static str);
    #[async_trait::async_trait]
    impl LlmExecutor for LabeledModel {
        async fn infer(
            &self,
            _request: ChatRequest,
        ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
            Ok(ChatResponse {
                output: AssistantOutput::text(self.0),
                usage: None,
                stop_reason: None,
            })
        }
    }

    struct MapProvider(HashMap<String, Arc<dyn LlmExecutor>>);
    impl ExecutorProvider for MapProvider {
        fn executor_for(&self, model_ref: &str) -> Option<Arc<dyn LlmExecutor>> {
            self.0.get(model_ref).cloned()
        }
    }

    #[test]
    fn executor_for_resolves_through_the_provider_else_none() {
        let fast: Arc<dyn LlmExecutor> = Arc::new(LabeledModel("fast"));
        let mut map: HashMap<String, Arc<dyn LlmExecutor>> = HashMap::new();
        map.insert("fast-model".into(), fast.clone());
        let mut binding = ThreadModelBinding::new();
        binding.set_provider(Arc::new(MapProvider(map)));

        // A resolvable ref → the provider's executor (resolved per run, from the ref).
        assert!(Arc::ptr_eq(
            &binding.executor_for("fast-model").unwrap(),
            &fast
        ));
        // An unknown ref → None → the caller falls back to the runtime's bound default.
        assert!(binding.executor_for("no-such").is_none());
    }

    #[test]
    fn re_registering_a_thread_replaces_the_binding() {
        // The per-turn override re-stages the thread's model: the LAST register wins, so
        // a turn cannot keep running a stale prior model ref. `model_ref` reflects the
        // latest registration (the executor itself is resolved separately via
        // `executor_for` at run time).
        let mut binding = ThreadModelBinding::new();
        let mut map: HashMap<String, Arc<dyn LlmExecutor>> = HashMap::new();
        map.insert("model-a".into(), Arc::new(LabeledModel("a")) as Arc<dyn LlmExecutor>);
        map.insert("model-b".into(), Arc::new(LabeledModel("b")) as Arc<dyn LlmExecutor>);
        binding.set_provider(Arc::new(MapProvider(map)));

        binding.register("t", "model-a");
        assert_eq!(binding.model_ref("t", "default-model"), "model-a");
        // Re-register (per-turn override) → the new ref replaces the old one.
        binding.register("t", "model-b");
        assert_eq!(binding.model_ref("t", "default-model"), "model-b");
        assert_eq!(binding.override_for("t").as_deref(), Some("model-b"));
    }

    #[test]
    fn no_provider_means_executor_for_is_always_none() {
        let binding = ThreadModelBinding::new();
        assert!(binding.executor_for("whatever").is_none());
    }

    #[test]
    fn override_for_returns_the_staged_override_else_none() {
        let binding = ThreadModelBinding::new();
        assert!(binding.override_for("t").is_none());
        binding.register("t", "fast-model");
        assert_eq!(binding.override_for("t").as_deref(), Some("fast-model"));
    }
}
