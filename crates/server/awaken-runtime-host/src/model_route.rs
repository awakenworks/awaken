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

use awaken_run_ingress::ModelAccessRef;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::llm::LlmExecutor;

/// Resolves a bound model ref to its executor (R1). `None` from `executor_for`
/// means "use the host default" — a deployment with one fixed model registers no
/// provider and is unaffected.
pub trait ExecutorProvider: Send + Sync {
    /// The executor for `model_ref`, or `None` to fall back to the host default.
    fn executor_for(&self, model_ref: &str) -> Option<Arc<dyn LlmExecutor>>;

    /// Resolve and pin the non-secret provider/route/credential binding at
    /// dispatch admission. Providers without dynamic credentials return `None`.
    fn model_access_for(&self, _model_ref: &str) -> Result<Option<ModelAccessRef>, String> {
        Ok(None)
    }

    /// Pin every model-access candidate this activation may use. Providers that
    /// only support one model inherit the exact-primary behavior.
    fn model_access_for_activation(
        &self,
        activation: &RunActivation,
    ) -> Result<Option<ModelAccessRef>, String> {
        self.model_access_for(activation.effective_model_ref())
    }

    /// Resolve one durable run, including its opaque model-access capability.
    /// Existing local-credential providers inherit the compatibility default and
    /// ignore the capability. Secretless gateway providers override this method
    /// and build an executor that presents/renews the referenced grant without
    /// exposing a provider key to the worker.
    fn executor_for_run(
        &self,
        model_ref: &str,
        model_access: Option<&ModelAccessRef>,
    ) -> Option<Arc<dyn LlmExecutor>> {
        let _ = model_access;
        self.executor_for(model_ref)
    }
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

    pub(crate) fn model_access_for_activation(
        &self,
        activation: &RunActivation,
    ) -> Result<Option<ModelAccessRef>, String> {
        match &self.provider {
            Some(provider) => provider.model_access_for_activation(activation),
            None => Ok(None),
        }
    }

    pub(crate) fn executor_for_activation(
        &self,
        activation: &RunActivation,
    ) -> Result<Option<Arc<dyn LlmExecutor>>, String> {
        let Some(provider) = &self.provider else {
            return Ok(None);
        };
        let access = provider.model_access_for_activation(activation)?;
        Ok(provider.executor_for_run(activation.effective_model_ref(), access.as_ref()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::run::Id as RunId;
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse};
    use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
    use awaken_runtime_contract::snapshot::{
        AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
    };

    fn activation(model_ref: &str) -> RunActivation {
        RunActivation::new(
            RunId("run".into()),
            ThreadId("thread".into()),
            ExecutableAgentSnapshot {
                id: ExecutableAgentSnapshotId("snapshot".into()),
                root_agent_id: AgentId("agent".into()),
                resolved_spec: ResolvedSpec {
                    catalog_fingerprint: CatalogFingerprint("catalog".into()),
                    instructions: String::new(),
                    max_steps: 1,
                    delegation_limits: Default::default(),
                    model_binding: ModelBinding::new("provider", model_ref, "backend"),
                    model_candidates: Vec::new(),
                    tool_descriptors: Vec::new(),
                    plugin_ids: Vec::new(),
                    plugin_config: Default::default(),
                    context_policy: Default::default(),
                    tool_presentation: Default::default(),
                },
                fingerprint: CatalogFingerprint("catalog".into()),
            },
            Vec::new(),
        )
    }

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
            &binding
                .executor_for_activation(&activation("fast-model"))
                .unwrap()
                .unwrap(),
            &fast
        ));
        // An unknown ref → None → the caller falls back to the runtime's bound default.
        assert!(
            binding
                .executor_for_activation(&activation("no-such"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn re_registering_a_thread_replaces_the_binding() {
        // The per-turn override re-stages the thread's model: the LAST register wins, so
        // a turn cannot keep running a stale prior model ref. `model_ref` reflects the
        // latest registration (the executor itself is resolved separately via
        // `executor_for` at run time).
        let mut binding = ThreadModelBinding::new();
        let mut map: HashMap<String, Arc<dyn LlmExecutor>> = HashMap::new();
        map.insert(
            "model-a".into(),
            Arc::new(LabeledModel("a")) as Arc<dyn LlmExecutor>,
        );
        map.insert(
            "model-b".into(),
            Arc::new(LabeledModel("b")) as Arc<dyn LlmExecutor>,
        );
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
        assert!(
            binding
                .executor_for_activation(&activation("whatever"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn durable_run_resolution_forwards_the_opaque_gateway_grant() {
        struct GatewayProvider {
            seen: Arc<Mutex<Option<ModelAccessRef>>>,
            executor: Arc<dyn LlmExecutor>,
            grant: ModelAccessRef,
        }

        impl ExecutorProvider for GatewayProvider {
            fn executor_for(&self, _model_ref: &str) -> Option<Arc<dyn LlmExecutor>> {
                None
            }

            fn executor_for_run(
                &self,
                model_ref: &str,
                model_access: Option<&ModelAccessRef>,
            ) -> Option<Arc<dyn LlmExecutor>> {
                assert_eq!(model_ref, "gateway-model");
                *self.seen.lock().expect("grant capture mutex") = model_access.cloned();
                Some(self.executor.clone())
            }

            fn model_access_for_activation(
                &self,
                _activation: &RunActivation,
            ) -> Result<Option<ModelAccessRef>, String> {
                Ok(Some(self.grant.clone()))
            }
        }

        let seen = Arc::new(Mutex::new(None));
        let executor: Arc<dyn LlmExecutor> = Arc::new(LabeledModel("gateway"));
        let grant = ModelAccessRef::new("cloud-gateway", "grant-42");
        let mut binding = ThreadModelBinding::new();
        binding.set_provider(Arc::new(GatewayProvider {
            seen: seen.clone(),
            executor: executor.clone(),
            grant: grant.clone(),
        }));

        let resolved = binding
            .executor_for_activation(&activation("gateway-model"))
            .unwrap()
            .expect("gateway provider resolves the run");
        assert!(Arc::ptr_eq(&resolved, &executor));
        assert_eq!(*seen.lock().expect("grant capture mutex"), Some(grant));
    }

    #[test]
    fn override_for_returns_the_staged_override_else_none() {
        let binding = ThreadModelBinding::new();
        assert!(binding.override_for("t").is_none());
        binding.register("t", "fast-model");
        assert_eq!(binding.override_for("t").as_deref(), Some("fast-model"));
    }
}
