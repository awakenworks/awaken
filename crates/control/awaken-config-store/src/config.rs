//! The config domain's authoring aggregate.

use std::collections::BTreeMap;

use awaken_runtime_contract::delegation::DelegationLimits;
use awaken_runtime_contract::resolved::{ContextPolicy, ModelBinding};
use awaken_runtime_contract::tool::ToolRecoveryPolicy;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// How an agent config's model is chosen at authoring time (ADR-0052 D5). This is
/// the *source* selection, distinct from the resolved concrete [`ModelBinding`] the
/// runtime consumes: the resolver collapses [`Auto`](ModelSelection::Auto) to a
/// first-offering at publish, and passes a [`Pinned`](ModelSelection::Pinned)
/// binding through untouched. The two variants *name* the two intents at the type
/// level, so no reader has to know that an absent value carries behavior.
///
/// Wire compatibility is deliberate: a `Pinned` binding serializes as the bare flat
/// triple it always was (`{provider_identity_ref, model_ref, backend_ref}`), so
/// every config authored before this type — and its content-address fingerprint —
/// is byte-identical. Only `Auto` is new, serialized as `{"mode":"auto"}`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ModelSelection {
    /// Resolve to a first provider-backed offering at publish (the default). The
    /// reconciler re-resolves these on a model-catalog change (ADR-0052 D5).
    #[default]
    Auto,
    /// The operator's explicit concrete binding — never overwritten by resolution.
    Pinned(ModelBinding),
}

impl ModelSelection {
    /// A pinned binding from its three refs (ergonomic constructor for the many
    /// call sites that authored a concrete `ModelBinding::new(...)`).
    pub fn pinned(
        provider_identity_ref: impl Into<String>,
        model_ref: impl Into<String>,
        backend_ref: impl Into<String>,
    ) -> Self {
        ModelSelection::Pinned(ModelBinding::new(
            provider_identity_ref,
            model_ref,
            backend_ref,
        ))
    }

    /// The concrete binding if pinned, `None` if still `Auto`. `compile` requires a
    /// resolved binding, so `None` here is the fail-closed "resolve me first" signal.
    #[must_use]
    pub fn resolved(&self) -> Option<&ModelBinding> {
        match self {
            ModelSelection::Pinned(binding) => Some(binding),
            ModelSelection::Auto => None,
        }
    }

    /// Whether the selection is still `Auto` (the reconciler re-resolves these).
    #[must_use]
    pub fn is_auto(&self) -> bool {
        matches!(self, ModelSelection::Auto)
    }
}

impl From<ModelBinding> for ModelSelection {
    fn from(binding: ModelBinding) -> Self {
        ModelSelection::Pinned(binding)
    }
}

impl Serialize for ModelSelection {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            // Byte-identical to the historic flat triple, so existing configs and
            // their fingerprints are unchanged.
            ModelSelection::Pinned(binding) => binding.serialize(serializer),
            ModelSelection::Auto => {
                use serde::ser::SerializeMap;
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("mode", "auto")?;
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for ModelSelection {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Peek the shape: `{"mode":"auto"}` is Auto; anything else is the flat
        // triple (back-compat) or a `mode:"pinned"` triple, both decoding to Pinned.
        let value = serde_json::Value::deserialize(deserializer)?;
        if value.get("mode").and_then(serde_json::Value::as_str) == Some("auto") {
            return Ok(ModelSelection::Auto);
        }
        let binding =
            serde_json::from_value::<ModelBinding>(value).map_err(serde::de::Error::custom)?;
        Ok(ModelSelection::Pinned(binding))
    }
}

/// A declarative agent configuration, identified by `id`. This is the config
/// domain's source of truth; the runtime never edits it — it consumes only the
/// compiled snapshot (ADR-0031). Field order is the canonical serialization order
/// used for the publication fingerprint, so it must stay stable.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentConfig {
    pub id: String,
    pub instructions: String,
    pub max_steps: usize,
    /// Limits for child Runs initiated through an Agent tool. Defaults preserve
    /// existing configs; non-default values enter the publication fingerprint.
    #[serde(default, skip_serializing_if = "delegation_limits_are_default")]
    pub delegation_limits: DelegationLimits,
    /// The model selection (ADR-0052 D5): `Auto` (resolve at publish) or a
    /// `Pinned` concrete binding. Serializes wire-identically to the historic flat
    /// triple when pinned, so the field name and the publication fingerprint of
    /// every pre-existing config are unchanged.
    pub model_binding: ModelSelection,
    pub tool_ids: Vec<String>,
    /// Plugins active for this agent, by id. A plugin installed on the runtime
    /// contributes only when listed here (G30).
    #[serde(default)]
    pub plugin_ids: Vec<String>,
    /// Per-plugin configuration sections, keyed by plugin id. `BTreeMap` keeps the
    /// serialization deterministic for the publication fingerprint. Each active
    /// plugin reads its own section at resolve; an absent section means defaults.
    #[serde(default)]
    pub plugin_config: BTreeMap<String, serde_json::Value>,
    /// How the model-visible context window is bounded (default
    /// [`ContextPolicy::KeepAll`]). Appended last so it does not reorder the
    /// existing canonical serialization; `#[serde(default)]` keeps configs
    /// authored before this field loadable.
    #[serde(default)]
    pub context_policy: ContextPolicy,
    /// Glob patterns (`*` wildcard) selecting additional tools from the catalog by
    /// id at compile — a permissive selector that complements the exact `tool_ids`.
    /// Unlike a `tool_id`, a pattern that matches nothing is not an error (it is a
    /// filter, not a reference). Appended last with `skip_serializing_if` so an
    /// empty set serializes to nothing and keeps prior fingerprints byte-identical;
    /// a non-empty set enters the content address like any other config field.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_patterns: Vec<String>,
    /// Ordered model-pool fallbacks (#1): tried after `model_binding` when a
    /// candidate fails cleanly, so an agent survives a model outage. Appended last
    /// with `skip_serializing_if` so a single-model config's fingerprint stays
    /// byte-identical; a non-empty pool enters the content address like any field.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_candidates: Vec<ModelBinding>,
    /// Managed-Agent identity/wire fields, carried so the config plane's agent
    /// object stays consistent with the SDK `/v1/agents` object (name / model /
    /// system / tools / mcp_servers / skills / multiagent / metadata). These are
    /// authoring metadata — the runtime consumes only the compiled fields above —
    /// so they are `skip_serializing_if`-empty to keep prior fingerprints identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multiagent: Option<serde_json::Value>,
    /// Soft-deletion lifecycle of the authoring aggregate. Archived Agents remain
    /// readable (including revision history) but cannot be published or selected
    /// for new execution. Kept on the aggregate rather than hidden in metadata so
    /// every adapter enforces the same invariant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<String>,
    /// How this agent's tools are presented to the model (ADR-0053): per-tool alias /
    /// description override / defer. Appended last with `skip_serializing_if`-empty so a
    /// config with no overrides serializes to nothing and keeps its prior fingerprint
    /// byte-identical; a non-empty set enters the content address like any other field.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_overrides: Vec<ToolOverride>,
    /// Per-tool crash recovery policy, keyed by canonical tool id. This selects
    /// behavior but never grants capability: the runtime checks it against the
    /// executable tool and fails closed if the configuration widens it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub recovery_policies: BTreeMap<String, ToolRecoveryPolicy>,
    /// The agent's compaction strategy (WHEN to compact) over the model's context window.
    /// Appended last with `skip_serializing_if`-none so an agent that sets none serializes to
    /// nothing and keeps its prior fingerprint byte-identical. At publish, the effective
    /// trigger is derived from the resolved model (`context_window` − `max_output_tokens`
    /// headroom) and stamped into both realizations' `plugin_config` slots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<CompactionStrategy>,
}

fn delegation_limits_are_default(limits: &DelegationLimits) -> bool {
    limits == &DelegationLimits::default()
}

/// An agent's compaction **strategy** — WHEN to compact its context. This is authored agent
/// config: the model provides the context-length *capability* (`context_window`); the agent
/// decides the *trigger* within it, optionally overriding the derived default. The runtime
/// never sees this type — at publish the effective window is computed and stamped into
/// `plugin_config` (native compact ext + ACP `compact_window`), which is what realizers read.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionStrategy {
    /// The agent's chosen trigger window in tokens; `None` derives it from the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<u32>,
    /// Recent turns kept verbatim past the injected summary; `None` uses the realizer default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_recent: Option<u32>,
}

impl CompactionStrategy {
    /// The effective compaction trigger from a model's window attributes — the value both
    /// realizations use (native `compact.max_tokens` at ratio 1.0, ACP `compact_window`):
    /// - the agent's `window` is honored but **clamped** to the usable input budget
    ///   (`context_window` − the reserved output ceiling `max_output_tokens`), so input +
    ///   output never exceeds the model's limit;
    /// - unset, it defaults to 3/4 of that usable budget;
    /// - `None` when the model publishes no `context_window` and the agent set none (no basis).
    #[must_use]
    pub fn effective_window(
        &self,
        context_window: Option<u32>,
        max_output_tokens: Option<u32>,
    ) -> Option<u32> {
        let budget = context_window.map(|cw| cw.saturating_sub(max_output_tokens.unwrap_or(0)));
        match (self.window, budget) {
            (Some(w), Some(b)) => Some(w.min(b)),
            (Some(w), None) => Some(w),
            (None, Some(b)) => Some(b / 4 * 3),
            (None, None) => None,
        }
    }
}

/// A per-tool presentation override (ADR-0053): rename and/or re-describe a selected
/// tool for the model, and/or `defer` sending its schema until the model opens it.
/// `target` is the tool's **canonical** id — a catalog id or an MCP `mcp__<server>__<tool>`
/// id — so overrides apply to static and MCP tools uniformly. Authoring-only: `compile`
/// validates each target against the agent's selected tools and projects the set into
/// the runtime [`ToolPresentation`](awaken_runtime_contract::resolved::ToolPresentation).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolOverride {
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub defer: bool,
}

#[cfg(test)]
mod compaction_tests {
    use super::CompactionStrategy;

    #[test]
    fn effective_window_defaults_to_three_quarters_of_the_usable_budget() {
        // Usable budget = context_window − max_output_tokens = 100k − 20k = 80k; default 3/4.
        let s = CompactionStrategy::default();
        assert_eq!(
            s.effective_window(Some(100_000), Some(20_000)),
            Some(60_000)
        );
    }

    #[test]
    fn effective_window_honors_the_override_but_clamps_to_the_budget() {
        let under = CompactionStrategy {
            window: Some(50_000),
            keep_recent: None,
        };
        assert_eq!(
            under.effective_window(Some(100_000), Some(20_000)),
            Some(50_000)
        );
        // Over the usable budget → clamped (input + output can't exceed the model's limit).
        let over = CompactionStrategy {
            window: Some(500_000),
            keep_recent: None,
        };
        assert_eq!(
            over.effective_window(Some(100_000), Some(20_000)),
            Some(80_000)
        );
    }

    #[test]
    fn effective_window_trusts_the_agent_when_the_model_publishes_no_window() {
        assert_eq!(
            CompactionStrategy {
                window: Some(40_000),
                keep_recent: None
            }
            .effective_window(None, None),
            Some(40_000)
        );
        // No model budget AND no agent choice → no basis to compact.
        assert_eq!(
            CompactionStrategy::default().effective_window(None, None),
            None
        );
    }
}
