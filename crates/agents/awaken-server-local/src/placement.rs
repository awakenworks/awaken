//! Config-driven static hand placement (ADR-0046) — the open default
//! [`ToolExecutorProvider`].
//!
//! This is the self-hosted brain–hand split: configuration names which agent's
//! tool calls run on which hand. Each worker's channel is established **once at
//! startup** and wrapped in a `RemoteToolExecutor`; [`provide`](ConfigToolExecutorProvider::provide)
//! is then a pure per-run lookup — it opens no connection and consults no
//! registry/lease/scheduler (the port stays placement-mechanism-agnostic, G16).
//! A run matching no entry returns `None`, so the kernel's in-process
//! `LocalToolExecutor` runs its tools unchanged.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::tool::{ToolExecutor, ToolExecutorProvider};

/// One placement rule: runs whose root agent matches `agent_id` run their tools
/// on `executor`. `agent_id: None` is a catch-all (place it last) — every run it
/// reaches is placed on `executor`.
pub struct PlacementEntry {
    /// The root agent id this entry places, or `None` to match any run.
    pub agent_id: Option<String>,
    /// The hand this entry places matched runs on (a channel opened at startup).
    pub executor: Arc<dyn ToolExecutor>,
}

impl PlacementEntry {
    /// Place runs of `agent_id` on `executor`.
    pub fn for_agent(agent_id: impl Into<String>, executor: Arc<dyn ToolExecutor>) -> Self {
        Self {
            agent_id: Some(agent_id.into()),
            executor,
        }
    }

    /// Place every reaching run on `executor` (a catch-all; list it last).
    pub fn any(executor: Arc<dyn ToolExecutor>) -> Self {
        Self {
            agent_id: None,
            executor,
        }
    }

    fn matches(&self, activation: &RunActivation) -> bool {
        match &self.agent_id {
            None => true,
            Some(id) => *id == activation.snapshot.root_agent_id.0,
        }
    }
}

/// The open default hand-placement provider: a fixed, ordered list of
/// [`PlacementEntry`] rules resolved once at startup (ADR-0046 D2). `provide`
/// returns the first matching entry's executor, or `None` (→ in-process default).
pub struct ConfigToolExecutorProvider {
    entries: Vec<PlacementEntry>,
}

impl ConfigToolExecutorProvider {
    /// A provider over `entries`, matched in order (first match wins).
    #[must_use]
    pub fn new(entries: Vec<PlacementEntry>) -> Self {
        Self { entries }
    }
}

#[async_trait]
impl ToolExecutorProvider for ConfigToolExecutorProvider {
    async fn provide(&self, activation: &RunActivation) -> Option<Arc<dyn ToolExecutor>> {
        // Static config: a pure lookup that resolves in a ready future — the async
        // seam (ADR-0046, G2) exists for dynamic drivers that must await I/O.
        self.entries
            .iter()
            .find(|entry| entry.matches(activation))
            .map(|entry| entry.executor.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
    use awaken_agent_contract::agent::run::Id as RunId;
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_runtime_contract::llm::ToolCall;
    use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
    use awaken_runtime_contract::snapshot::{
        AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
    };
    use awaken_runtime_contract::tool::{ToolError, ToolOutput};

    /// A `ToolExecutor` that records nothing and returns a fixed marker, tagged so
    /// a test can tell which placed executor `provide` returned.
    struct TaggedExecutor(&'static str);
    #[async_trait]
    impl ToolExecutor for TaggedExecutor {
        async fn invoke(&self, call: &ToolCall) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::ok(call.call_id.clone(), self.0))
        }
    }

    fn activation_for(agent_id: &str) -> RunActivation {
        RunActivation {
            run_id: RunId("r".into()),
            thread_id: ThreadId("t".into()),
            snapshot: ExecutableAgentSnapshot {
                id: ExecutableAgentSnapshotId("s".into()),
                root_agent_id: AgentId(agent_id.into()),
                resolved_spec: ResolvedSpec {
                    catalog_fingerprint: CatalogFingerprint("fp".into()),
                    instructions: String::new(),
                    max_steps: 4,
                    model_binding: ModelBinding::new("p", "m", "echo"),
                    tool_descriptors: Vec::new(),
                    plugin_ids: Vec::new(),
                    plugin_config: Default::default(),
                    context_policy: Default::default(),
                },
                fingerprint: CatalogFingerprint("fp".into()),
            },
            input: vec![Message::text(MessageId("u".into()), Role::User, "go")],
            trace: Default::default(),
        }
    }

    async fn returned_marker(exec: &Arc<dyn ToolExecutor>) -> String {
        exec.invoke(&ToolCall {
            call_id: "c".into(),
            tool_id: "bash".into(),
            arguments: serde_json::json!({}),
        })
        .await
        .unwrap()
        .content
    }

    #[tokio::test]
    async fn places_a_matched_agent_and_falls_through_for_the_rest() {
        let provider = ConfigToolExecutorProvider::new(vec![PlacementEntry::for_agent(
            "remote-agent",
            Arc::new(TaggedExecutor("HAND")),
        )]);

        // The named agent is placed on the hand.
        let placed = provider
            .provide(&activation_for("remote-agent"))
            .await
            .unwrap();
        assert_eq!(returned_marker(&placed).await, "HAND");

        // Any other agent is unplaced → None → the kernel's in-process executor.
        assert!(
            provider
                .provide(&activation_for("local-agent"))
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn first_matching_entry_wins_and_catch_all_places_everything() {
        let provider = ConfigToolExecutorProvider::new(vec![
            PlacementEntry::for_agent("special", Arc::new(TaggedExecutor("SPECIAL"))),
            PlacementEntry::any(Arc::new(TaggedExecutor("DEFAULT"))),
        ]);

        // The specific rule wins for its agent...
        let special = provider.provide(&activation_for("special")).await.unwrap();
        assert_eq!(returned_marker(&special).await, "SPECIAL");

        // ...and the catch-all places every other run.
        let other = provider
            .provide(&activation_for("anything-else"))
            .await
            .unwrap();
        assert_eq!(returned_marker(&other).await, "DEFAULT");
    }

    #[tokio::test]
    async fn no_entries_places_nothing() {
        let provider = ConfigToolExecutorProvider::new(Vec::new());
        assert!(provider.provide(&activation_for("a")).await.is_none());
    }
}
