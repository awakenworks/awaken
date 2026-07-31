//! Config-driven static hand placement (ADR-0046) — the open default
//! [`ToolExecutorProvider`].
//!
//! This is the self-hosted brain–hand split: configuration names which agent's
//! tool calls run on which hand. Each worker's channel is established **once at
//! startup** and wrapped in a `RemoteToolExecutor`; [`provide`](ConfigToolExecutorProvider::provide)
//! is then a pure per-run lookup — it opens no connection and consults no
//! registry/lease/scheduler (the port stays placement-mechanism-agnostic, G16).
//! A run matching no entry returns `Ok(None)`, so the kernel's in-process
//! `LocalToolExecutor` runs its tools unchanged.

use std::sync::Arc;
use std::{collections::BTreeMap, fmt};

use async_trait::async_trait;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::tool::{
    ToolExecutor, ToolExecutorProvider, ToolExecutorSelectionError,
};

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
    policy: PlacementPolicy,
}

enum PlacementPolicy {
    Static(Vec<PlacementEntry>),
    Declared {
        source: Arc<dyn DeclaredHandSource>,
        executors: BTreeMap<String, Arc<dyn ToolExecutor>>,
    },
}

/// Read-only projection of the config domain's current logical Hand intent.
pub trait DeclaredHandSource: Send + Sync {
    fn declared_hand(&self, agent_id: &str) -> Result<Option<String>, String>;
}

impl fmt::Debug for ConfigToolExecutorProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConfigToolExecutorProvider")
            .finish_non_exhaustive()
    }
}

impl ConfigToolExecutorProvider {
    /// A provider over `entries`, matched in order (first match wins).
    #[must_use]
    pub fn new(entries: Vec<PlacementEntry>) -> Self {
        Self {
            policy: PlacementPolicy::Static(entries),
        }
    }

    /// Join config-owned logical Hand ids to deployment-owned live executors.
    #[must_use]
    pub fn from_declared_hands(
        source: Arc<dyn DeclaredHandSource>,
        executors: BTreeMap<String, Arc<dyn ToolExecutor>>,
    ) -> Self {
        Self {
            policy: PlacementPolicy::Declared { source, executors },
        }
    }
}

/// Resolve every deployment Hand connection exactly once at startup.
///
/// Runs receive only the resulting executor table; neither the provider nor
/// Runtime performs discovery or dialing on an attempt.
pub async fn connect_declared_hands(
    plans: &BTreeMap<String, awaken_connection_plan::ConnectionPlan>,
) -> Result<BTreeMap<String, Arc<dyn ToolExecutor>>, String> {
    use awaken_connection_plan::ChannelFactory as _;

    let mut executors = BTreeMap::new();
    for (hand_id, plan) in plans {
        let channel = awaken_connection_plan::TokioChannelFactory
            .connect(plan)
            .await
            .map_err(|error| format!("connect declared Hand `{hand_id}`: {error}"))?;
        executors.insert(
            hand_id.clone(),
            Arc::new(awaken_tool_relay::RemoteToolExecutor::new(channel)) as Arc<dyn ToolExecutor>,
        );
    }
    Ok(executors)
}

#[async_trait]
impl ToolExecutorProvider for ConfigToolExecutorProvider {
    async fn provide(
        &self,
        activation: &RunActivation,
    ) -> Result<Option<Arc<dyn ToolExecutor>>, ToolExecutorSelectionError> {
        // Static config: a pure lookup that resolves in a ready future — the async
        // seam (ADR-0046, G2) exists for dynamic drivers that must await I/O.
        match &self.policy {
            PlacementPolicy::Static(entries) => Ok(entries
                .iter()
                .find(|entry| entry.matches(activation))
                .map(|entry| entry.executor.clone())),
            PlacementPolicy::Declared { source, executors } => {
                let agent_id = &activation.snapshot.root_agent_id.0;
                let declaration = source
                    .declared_hand(agent_id)
                    .map_err(ToolExecutorSelectionError::Policy)?;
                let Some(hand_id) = declaration else {
                    return Ok(None);
                };
                executors.get(&hand_id).cloned().map(Some).ok_or_else(|| {
                    ToolExecutorSelectionError::Unavailable(format!(
                        "Agent `{agent_id}` declares unknown Hand `{hand_id}`"
                    ))
                })
            }
        }
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
    struct Declared(BTreeMap<String, String>);

    impl DeclaredHandSource for Declared {
        fn declared_hand(&self, agent_id: &str) -> Result<Option<String>, String> {
            Ok(self.0.get(agent_id).cloned())
        }
    }
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
                metadata: Default::default(),
                root_agent_id: AgentId(agent_id.into()),
                resolved_spec: ResolvedSpec {
                    model_candidates: Vec::new(),
                    catalog_fingerprint: CatalogFingerprint("fp".into()),
                    instructions: String::new(),
                    max_steps: 4,
                    delegation_limits: Default::default(),
                    model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                        ModelBinding::new("p", "m", "echo"),
                    ),
                    tool_descriptors: Vec::new(),
                    plugin_ids: Vec::new(),
                    plugin_config: Default::default(),
                    context_policy: Default::default(),
                    tool_presentation: Default::default(),
                },
                fingerprint: CatalogFingerprint("fp".into()),
            },
            input: vec![Message::text(MessageId("u".into()), Role::User, "go")],
            delegation_origin: None,
            model_ref_override: None,
            data_subject_id: None,
            tool_capability_narrowing: Default::default(),
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
        .text()
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
            .unwrap()
            .unwrap();
        assert_eq!(returned_marker(&placed).await, "HAND");

        // Any other agent is unplaced → None → the kernel's in-process executor.
        assert!(
            provider
                .provide(&activation_for("local-agent"))
                .await
                .unwrap()
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
        let special = special.unwrap();
        assert_eq!(returned_marker(&special).await, "SPECIAL");

        // ...and the catch-all places every other run.
        let other = provider
            .provide(&activation_for("anything-else"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(returned_marker(&other).await, "DEFAULT");
    }

    #[tokio::test]
    async fn no_entries_places_nothing() {
        let provider = ConfigToolExecutorProvider::new(Vec::new());
        assert!(
            provider
                .provide(&activation_for("a"))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn declared_hand_join_is_fail_closed_and_snapshot_independent() {
        // Cause/effect graph:
        // C1 no declaration -> E1 ordinary local fallback; C2 declaration and
        // exact deployment executor -> E2 place there; C3 declaration without
        // executor -> E3 fail before the run; C4 source ambiguity -> E4 policy
        // failure. The activation snapshot contains no placement field.
        //
        // Decision table:
        // | D1 | none       | any     | None        |
        // | D2 | hand-east  | present | HAND        |
        // | D3 | hand-gone  | absent  | Unavailable |
        let provider = ConfigToolExecutorProvider::from_declared_hands(
            Arc::new(Declared(BTreeMap::from([
                ("remote".into(), "hand-east".into()),
                ("missing".into(), "hand-gone".into()),
            ]))),
            BTreeMap::from([(
                "hand-east".into(),
                Arc::new(TaggedExecutor("HAND")) as Arc<dyn ToolExecutor>,
            )]),
        );

        assert!(
            provider
                .provide(&activation_for("local"))
                .await
                .unwrap()
                .is_none(),
            "D1"
        );
        let placed = provider
            .provide(&activation_for("remote"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(returned_marker(&placed).await, "HAND", "D2");
        assert!(
            matches!(
                provider.provide(&activation_for("missing")).await,
                Err(ToolExecutorSelectionError::Unavailable(_))
            ),
            "D3"
        );
    }
}
