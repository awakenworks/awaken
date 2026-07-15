use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunActivation {
    pub run_id: awaken_agent_contract::agent::run::Id,
    pub thread_id: awaken_agent_contract::agent::thread::Id,
    pub snapshot: crate::snapshot::ExecutableAgentSnapshot,
    pub input: Vec<awaken_agent_contract::agent::message::Message>,
    /// Per-run model override (R5): the model ref to run THIS attempt on, when it
    /// differs from the agent's published binding. Deliberately OFF the fingerprinted
    /// snapshot — a per-turn model switch is a run-time choice, not a catalog change,
    /// so it must not mint a new `catalog_fingerprint` (mirrors how display metadata
    /// is excluded from the content address). Absent ⇒ the run uses its snapshot's
    /// `model_binding.model_ref`. This names *which* model to run; the runtime never
    /// sees *how* it is reached — that is the provider's job at the resolve seam.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_ref_override: Option<String>,
}

impl RunActivation {
    /// A fresh activation carrying only the runtime-core inputs. *How* the run
    /// reaches its model is not here: the resolve seam turns the run's model ref into
    /// a concrete provider (executor) before execution — the runtime only ever names a
    /// model and is handed the executor, never learning how the model is reached.
    ///
    /// Distributed *trace* propagation is NOT carried here either: the admitting request's
    /// W3C `traceparent` rides the ingress envelope (`RunExecutionRequest`) across
    /// the durable queue and is restored as the `wake.dispatch` span's remote
    /// parent, so a durably-drained run still nests under the trace that submitted
    /// it. The runtime core never reads a trace field.
    #[must_use]
    pub fn new(
        run_id: awaken_agent_contract::agent::run::Id,
        thread_id: awaken_agent_contract::agent::thread::Id,
        snapshot: crate::snapshot::ExecutableAgentSnapshot,
        input: Vec<awaken_agent_contract::agent::message::Message>,
    ) -> Self {
        Self {
            run_id,
            thread_id,
            snapshot,
            input,
            model_ref_override: None,
        }
    }

    #[cfg(test)]
    fn for_binding(binding_model_ref: &str) -> Self {
        use crate::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
        use crate::snapshot::{AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId};
        Self::new(
            awaken_agent_contract::agent::run::Id("r".into()),
            awaken_agent_contract::agent::thread::Id("t".into()),
            ExecutableAgentSnapshot {
                id: ExecutableAgentSnapshotId("s".into()),
                root_agent_id: AgentId("a".into()),
                resolved_spec: ResolvedSpec {
                    model_candidates: Vec::new(),
                    catalog_fingerprint: CatalogFingerprint("fp".into()),
                    instructions: String::new(),
                    max_steps: 4,
                    model_binding: ModelBinding::new("prov", binding_model_ref, "backend"),
                    tool_descriptors: Vec::new(),
                    plugin_ids: Vec::new(),
                    plugin_config: Default::default(),
                    context_policy: Default::default(),
                    tool_presentation: Default::default(),
                },
                fingerprint: CatalogFingerprint("fp".into()),
            },
            Vec::new(),
        )
    }

    /// Set the per-run model override (R5) — the model ref to run this attempt on.
    #[must_use]
    pub fn with_model_ref_override(mut self, model_ref: Option<String>) -> Self {
        self.model_ref_override = model_ref;
        self
    }

    /// The model ref this attempt runs on: its per-run override (R5) when set,
    /// otherwise the model its pinned snapshot binding names. This is the single
    /// input the resolve seam turns into a provider — the runtime never looks a model
    /// up, it is handed the resolved executor. A blank override is treated as absent.
    #[must_use]
    pub fn effective_model_ref(&self) -> &str {
        match self.model_ref_override.as_deref() {
            Some(m) if !m.is_empty() => m,
            _ => &self.snapshot.resolved_spec.model_binding.model_ref,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RunActivation;

    // Cause-effect coverage for `effective_model_ref`. Causes: the override is
    // present / blank / absent. Effect: the ref used is the override, else the
    // snapshot binding — a blank override collapses to "absent".
    #[test]
    fn effective_ref_is_the_override_when_present() {
        let a = RunActivation::for_binding("bound").with_model_ref_override(Some("chosen".into()));
        assert_eq!(a.effective_model_ref(), "chosen");
    }

    #[test]
    fn effective_ref_falls_back_to_the_binding_when_no_override() {
        let a = RunActivation::for_binding("bound");
        assert_eq!(a.effective_model_ref(), "bound");
    }

    #[test]
    fn a_blank_override_collapses_to_the_binding() {
        let a = RunActivation::for_binding("bound").with_model_ref_override(Some(String::new()));
        assert_eq!(a.effective_model_ref(), "bound");
    }

    /// The override is off the fingerprinted snapshot — switching it must not change
    /// the snapshot identity (a per-turn model switch is not a catalog change).
    #[test]
    fn overriding_the_model_does_not_touch_the_snapshot_fingerprint() {
        let base = RunActivation::for_binding("bound");
        let fp = base.snapshot.fingerprint.clone();
        let overridden = base.with_model_ref_override(Some("chosen".into()));
        assert_eq!(overridden.snapshot.fingerprint, fp);
    }
}
