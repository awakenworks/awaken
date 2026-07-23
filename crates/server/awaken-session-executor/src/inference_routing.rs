//! Per-thread model binding and snapshot-access materialization (R1/R2).
//!
//! The host once held a single fixed `LlmExecutor`; this module lifts that to a
//! per-thread binding so a session (and, with a per-turn override, a turn) selects
//! its published model. [`InferenceRouting`] holds the runtime materialization
//! port plus per-thread overrides. Configuration resolution is deliberately not
//! represented here. A composition without the port uses its explicitly bound
//! executor; a configured materialization failure is rejected rather than
//! silently selecting another route.

use std::sync::Arc;

use awaken_runtime_contract::CredentialRef;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::resolved::ResolvedModelCandidate;
use std::collections::BTreeSet;

/// Turns an admission-pinned, secret-free inference access descriptor into a live
/// executor. Selecting models and routes is deliberately outside this port; the
/// materializer may only consume the exact activation and access it is given.
pub trait InferenceExecutorMaterializer: Send + Sync {
    /// Access schemes this execution adapter can materialize. Worker composition
    /// derives its immutable capability manifest from this declaration so
    /// placement and materialization cannot be configured independently and
    /// drift. These are execution capabilities, not authorization grants.
    fn supported_access_schemes(&self) -> &'static [&'static str] {
        &[]
    }

    /// Exact worker-private credential revisions currently materializable by
    /// this adapter. The default is empty for host executors and shared-vault
    /// adapters. Implementations must never expose secret values or local paths.
    fn available_credential_refs(&self) -> BTreeSet<CredentialRef> {
        BTreeSet::new()
    }

    /// Materialize one configuration-pinned model/access pair. The implementation
    /// may inject referenced credential material, but must not resolve or select a
    /// different model, route, scope, or credential.
    fn materialize_pinned(
        &self,
        candidate: &ResolvedModelCandidate,
    ) -> Option<Arc<dyn LlmExecutor>>;

    /// Materialize exactly the pinned access for this activation. Returning
    /// `None` rejects the run; it never falls back to a different route.
    fn materialize(&self, activation: &RunActivation) -> Option<Arc<dyn LlmExecutor>> {
        let exact = activation
            .snapshot
            .resolved_spec
            .candidate_for_model(activation.effective_model_ref())?;
        self.materialize_pinned(exact)
    }
}

/// The host's per-thread model binding: which model ref each thread runs, and how
/// a ref becomes an executor.
pub(crate) struct InferenceRouting {
    materializer: Option<Arc<dyn InferenceExecutorMaterializer>>,
    slots: crate::session_slot::SessionRuntimeSlots,
}

impl InferenceRouting {
    pub(crate) fn new(slots: crate::session_slot::SessionRuntimeSlots) -> Self {
        Self {
            materializer: None,
            slots,
        }
    }

    pub(crate) fn set_materializer(
        &mut self,
        materializer: Arc<dyn InferenceExecutorMaterializer>,
    ) {
        self.materializer = Some(materializer);
    }

    pub(crate) fn materializer(&self) -> Option<Arc<dyn InferenceExecutorMaterializer>> {
        self.materializer.clone()
    }

    /// Bind `model_ref` to `thread` (R2/R5), staged before its first turn.
    /// Re-registering replaces the binding (the per-turn override re-stages).
    pub(crate) fn register(&self, thread: &str, model_ref: impl Into<String>) {
        self.slots
            .update(thread, |slot| slot.model_ref = Some(model_ref.into()));
    }

    /// The model ref bound to `thread`, or `default_ref`.
    pub(crate) fn model_ref(&self, thread: &str, default_ref: &str) -> String {
        self.slots
            .read(thread, |slot| slot.model_ref.clone())
            .flatten()
            .unwrap_or_else(|| default_ref.to_string())
    }

    /// The per-thread model override (R2/R5), if one was staged — `None` when the run
    /// uses its agent's published binding. Stamped onto the run's activation at
    /// delivery so the resolve seam sees it without consulting this map.
    pub(crate) fn override_for(&self, thread: &str) -> Option<String> {
        self.slots
            .read(thread, |slot| slot.model_ref.clone())
            .flatten()
    }

    pub(crate) fn executor_for_activation(
        &self,
        activation: &RunActivation,
    ) -> Result<Option<Arc<dyn LlmExecutor>>, String> {
        let Some(materializer) = &self.materializer else {
            return Ok(None);
        };
        materializer
            .materialize(activation)
            .map(Some)
            .ok_or_else(|| {
                format!(
                    "snapshot `{}` pinned inference access cannot materialize model `{}`",
                    activation.snapshot.id.0,
                    activation.effective_model_ref()
                )
            })
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
    use std::collections::HashMap;
    use std::sync::Mutex;

    fn routing() -> InferenceRouting {
        InferenceRouting::new(Default::default())
    }

    fn activation(model_ref: &str) -> RunActivation {
        RunActivation::new(
            RunId("run".into()),
            ThreadId("thread".into()),
            ExecutableAgentSnapshot {
                id: ExecutableAgentSnapshotId("snapshot".into()),
                metadata: Default::default(),
                root_agent_id: AgentId("agent".into()),
                resolved_spec: ResolvedSpec {
                    catalog_fingerprint: CatalogFingerprint("catalog".into()),
                    instructions: String::new(),
                    max_steps: 1,
                    delegation_limits: Default::default(),
                    model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                        ModelBinding::new("provider", model_ref, "backend"),
                    ),
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
    impl InferenceExecutorMaterializer for MapProvider {
        fn materialize_pinned(
            &self,
            candidate: &ResolvedModelCandidate,
        ) -> Option<Arc<dyn LlmExecutor>> {
            matches!(
                candidate.provisioning,
                awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor
            )
            .then(|| self.0.get(&candidate.binding.model_ref).cloned())
            .flatten()
        }
    }

    #[test]
    fn executor_for_resolves_through_the_provider_else_none() {
        let fast: Arc<dyn LlmExecutor> = Arc::new(LabeledModel("fast"));
        let mut map: HashMap<String, Arc<dyn LlmExecutor>> = HashMap::new();
        map.insert("fast-model".into(), fast.clone());
        let mut binding = routing();
        let provider = Arc::new(MapProvider(map));
        binding.set_materializer(provider);

        // A resolvable ref → the provider's executor (resolved per run, from the ref).
        assert!(Arc::ptr_eq(
            &binding
                .executor_for_activation(&activation("fast-model"))
                .unwrap()
                .unwrap(),
            &fast
        ));
        // An unknown published ref is rejected; it cannot fall back to the host model.
        let error = match binding.executor_for_activation(&activation("no-such")) {
            Ok(_) => panic!("unknown pinned model must be rejected"),
            Err(error) => error,
        };
        assert!(error.contains("cannot materialize model `no-such`"));
    }

    #[test]
    fn executor_selects_the_effective_model_from_a_publication_candidate_set() {
        let fast: Arc<dyn LlmExecutor> = Arc::new(LabeledModel("fast"));
        let slow: Arc<dyn LlmExecutor> = Arc::new(LabeledModel("slow"));
        let mut map: HashMap<String, Arc<dyn LlmExecutor>> = HashMap::new();
        map.insert("fast-model".into(), fast.clone());
        map.insert("slow-model".into(), slow);
        let mut routing = routing();
        routing.set_materializer(Arc::new(MapProvider(map)));

        let mut activation = activation("fast-model");
        activation.snapshot.resolved_spec.model_candidates.push(
            awaken_runtime_contract::resolved::ResolvedModelCandidate::host(ModelBinding::new(
                "provider",
                "slow-model",
                "backend",
            )),
        );

        assert!(Arc::ptr_eq(
            &routing
                .executor_for_activation(&activation)
                .unwrap()
                .unwrap(),
            &fast
        ));
    }

    #[test]
    fn re_registering_a_thread_replaces_the_binding() {
        // The per-turn override re-stages the thread's model: the LAST register wins, so
        // a turn cannot keep running a stale prior model ref. `model_ref` reflects the
        // latest registration (the executor itself is resolved separately via
        // `executor_for` at run time).
        let mut binding = routing();
        let mut map: HashMap<String, Arc<dyn LlmExecutor>> = HashMap::new();
        map.insert(
            "model-a".into(),
            Arc::new(LabeledModel("a")) as Arc<dyn LlmExecutor>,
        );
        map.insert(
            "model-b".into(),
            Arc::new(LabeledModel("b")) as Arc<dyn LlmExecutor>,
        );
        let provider = Arc::new(MapProvider(map));
        binding.set_materializer(provider);

        binding.register("t", "model-a");
        assert_eq!(binding.model_ref("t", "default-model"), "model-a");
        // Re-register (per-turn override) → the new ref replaces the old one.
        binding.register("t", "model-b");
        assert_eq!(binding.model_ref("t", "default-model"), "model-b");
        assert_eq!(binding.override_for("t").as_deref(), Some("model-b"));
    }

    #[test]
    fn no_provider_means_executor_for_is_always_none() {
        let binding = routing();
        assert!(
            binding
                .executor_for_activation(&activation("whatever"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn installed_materializer_rejects_an_unavailable_published_candidate() {
        let mut binding = routing();
        binding.set_materializer(Arc::new(MapProvider(HashMap::new())));
        let activation = activation("model-a");

        let error = match binding.executor_for_activation(&activation) {
            Ok(_) => panic!("snapshot without pinned access must be rejected"),
            Err(error) => error,
        };

        assert!(error.contains("cannot materialize model"));
    }

    #[test]
    fn durable_run_materialization_forwards_the_complete_candidate() {
        struct ReferenceMaterializer {
            seen: Arc<Mutex<Option<ResolvedModelCandidate>>>,
            executor: Arc<dyn LlmExecutor>,
        }

        impl InferenceExecutorMaterializer for ReferenceMaterializer {
            fn materialize_pinned(
                &self,
                candidate: &ResolvedModelCandidate,
            ) -> Option<Arc<dyn LlmExecutor>> {
                assert_eq!(candidate.binding.model_ref, "gateway-model");
                *self.seen.lock().expect("candidate capture mutex") = Some(candidate.clone());
                Some(self.executor.clone())
            }
        }

        let seen = Arc::new(Mutex::new(None));
        let executor: Arc<dyn LlmExecutor> = Arc::new(LabeledModel("gateway"));
        let mut binding = routing();
        let materializer = Arc::new(ReferenceMaterializer {
            seen: seen.clone(),
            executor: executor.clone(),
        });
        binding.set_materializer(materializer);

        let activation = activation("gateway-model");
        let candidate = activation.snapshot.resolved_spec.model_binding.clone();
        let resolved = binding
            .executor_for_activation(&activation)
            .unwrap()
            .expect("reference materializer resolves the run");
        assert!(Arc::ptr_eq(&resolved, &executor));
        assert_eq!(
            *seen.lock().expect("candidate capture mutex"),
            Some(candidate)
        );
    }

    #[test]
    fn override_for_returns_the_staged_override_else_none() {
        let binding = routing();
        assert!(binding.override_for("t").is_none());
        binding.register("t", "fast-model");
        assert_eq!(binding.override_for("t").as_deref(), Some("fast-model"));
    }
}
