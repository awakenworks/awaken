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

    /// The executor a thread's runs use: the provider's resolution of the thread's
    /// bound model ref, else `default_executor`. The single seam that replaced "the
    /// host has one fixed executor".
    pub(crate) fn resolve_executor(
        &self,
        thread: &str,
        default_ref: &str,
        default_executor: &Arc<dyn LlmExecutor>,
    ) -> Arc<dyn LlmExecutor> {
        let model_ref = self.model_ref(thread, default_ref);
        self.provider
            .as_ref()
            .and_then(|provider| provider.executor_for(&model_ref))
            .unwrap_or_else(|| default_executor.clone())
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
    fn binds_per_thread_model_else_default() {
        let default: Arc<dyn LlmExecutor> = Arc::new(LabeledModel("default"));
        let fast: Arc<dyn LlmExecutor> = Arc::new(LabeledModel("fast"));
        let mut map: HashMap<String, Arc<dyn LlmExecutor>> = HashMap::new();
        map.insert("fast-model".into(), fast.clone());
        let mut binding = ThreadModelBinding::new();
        binding.set_provider(Arc::new(MapProvider(map)));

        // No binding → host default.
        assert!(Arc::ptr_eq(
            &binding.resolve_executor("t-none", "default-model", &default),
            &default
        ));
        // Bound to a resolvable model → the provider's executor (per-session model).
        binding.register("t-fast", "fast-model");
        assert!(Arc::ptr_eq(
            &binding.resolve_executor("t-fast", "default-model", &default),
            &fast
        ));
        // Bound to an unknown model → provider yields None → fail safe to default.
        binding.register("t-unknown", "no-such");
        assert!(Arc::ptr_eq(
            &binding.resolve_executor("t-unknown", "default-model", &default),
            &default
        ));
    }

    #[test]
    fn unbound_thread_resolves_the_default_ref_through_the_provider() {
        // A single-model deployment registers its ONE model under the default ref and
        // binds no per-thread model. resolve_executor must still route the default ref
        // through the provider (not blindly return the host fallback), so the config
        // plane's executor for the default model is used.
        let host_fallback: Arc<dyn LlmExecutor> = Arc::new(LabeledModel("fallback"));
        let configured: Arc<dyn LlmExecutor> = Arc::new(LabeledModel("configured-default"));
        let mut map: HashMap<String, Arc<dyn LlmExecutor>> = HashMap::new();
        map.insert("default-model".into(), configured.clone());
        let mut binding = ThreadModelBinding::new();
        binding.set_provider(Arc::new(MapProvider(map)));

        // No per-thread binding, but the provider resolves the DEFAULT ref → its executor.
        assert!(Arc::ptr_eq(
            &binding.resolve_executor("t-unbound", "default-model", &host_fallback),
            &configured
        ));
    }

    #[test]
    fn re_registering_a_thread_replaces_the_binding() {
        // The per-turn override re-stages the thread's model: the LAST register wins, so
        // a turn cannot keep running a stale prior model ref.
        let default: Arc<dyn LlmExecutor> = Arc::new(LabeledModel("default"));
        let a: Arc<dyn LlmExecutor> = Arc::new(LabeledModel("a"));
        let b: Arc<dyn LlmExecutor> = Arc::new(LabeledModel("b"));
        let mut map: HashMap<String, Arc<dyn LlmExecutor>> = HashMap::new();
        map.insert("model-a".into(), a.clone());
        map.insert("model-b".into(), b.clone());
        let mut binding = ThreadModelBinding::new();
        binding.set_provider(Arc::new(MapProvider(map)));

        binding.register("t", "model-a");
        assert_eq!(binding.model_ref("t", "default-model"), "model-a");
        assert!(Arc::ptr_eq(
            &binding.resolve_executor("t", "default-model", &default),
            &a
        ));
        // Re-register (per-turn override) → the new ref replaces the old one.
        binding.register("t", "model-b");
        assert_eq!(binding.model_ref("t", "default-model"), "model-b");
        assert!(Arc::ptr_eq(
            &binding.resolve_executor("t", "default-model", &default),
            &b
        ));
    }

    #[test]
    fn no_provider_means_every_thread_uses_the_default() {
        let default: Arc<dyn LlmExecutor> = Arc::new(LabeledModel("default"));
        let binding = ThreadModelBinding::new();
        binding.register("t", "whatever");
        assert!(Arc::ptr_eq(
            &binding.resolve_executor("t", "m", &default),
            &default
        ));
    }
}
