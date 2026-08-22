//! The config domain's authoring aggregate.

use std::collections::BTreeMap;

use awaken_agent_contract::ModelTarget;
use awaken_runtime_contract::agent_bindings::InferenceOptions;
use awaken_runtime_contract::agent_bindings::ToolsetPolicy;
use awaken_runtime_contract::delegation::DelegationLimits;
use awaken_runtime_contract::resolved::{
    AcpBackend, AcpSessionConfiguration, ContextPolicy, ExactModelRef, ModelBinding, ToolDescriptor,
};
use awaken_runtime_contract::tool::ToolRecoveryPolicy;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// How an agent config's model is chosen at authoring time (ADR-0052 D5). This is
/// the *source* selection, distinct from the resolved concrete [`ModelBinding`] the
/// runtime consumes: the resolver collapses [`Auto`](ModelSelection::Auto) to a
/// first-offering at publish, and passes a [`Pinned`](ModelSelection::Pinned)
/// binding through untouched. [`BackendDefault`](ModelSelection::BackendDefault)
/// selects an external backend while leaving the model to that backend. The variants *name* the intents at the type
/// level, so no reader has to know that an absent value carries behavior.
///
/// Every variant uses the same explicit `mode` discriminator. In particular,
/// `Pinned` does not have a second untagged wire shape.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ModelSelection {
    /// Resolve to a first provider-backed offering at publish (the default). The
    /// reconciler re-resolves these on a model-catalog change (ADR-0052 D5).
    #[default]
    Auto,
    /// Resolve the explicitly named Workspace inference profile. Profiles are
    /// reusable policies, never an implicit Workspace-wide default.
    Profile { profile_id: String },
    /// Select one catalog model route without pretending it is already a
    /// publication-time binding. Qualifiers narrow catalog discovery; the
    /// backend names the executor that must consume the selected Offering.
    Target {
        target: ModelTarget,
        backend_ref: String,
        /// Adapter-native Session intent. Empty for the native executor and
        /// validated against live ACP capability evidence at publication.
        configuration: AcpSessionConfiguration,
    },
    /// Use this exact external backend and let it retain its own configured
    /// default model. Publication must resolve an exact Worker-local binding;
    /// this is never a fallback to [`Auto`](Self::Auto).
    BackendDefault {
        backend_ref: AcpBackend,
        configuration: AcpSessionConfiguration,
    },
    /// Use one exact model delivered through the external ACP together with
    /// adapter-native Session mode/options.
    BackendExact {
        backend_ref: AcpBackend,
        model_ref: ExactModelRef,
        configuration: AcpSessionConfiguration,
    },
    /// The operator's explicit concrete binding — never overwritten by resolution.
    Pinned(ModelBinding),
}

impl ModelSelection {
    /// The authored executor coordinate when one is already explicit.
    ///
    /// Auto/profile selections are native until publication resolves their
    /// model route. Backend-owned and pinned selections retain the exact
    /// encoded coordinate; this method never invents a second executor field.
    #[must_use]
    pub fn backend_ref(&self) -> Option<&str> {
        match self {
            Self::Auto | Self::Profile { .. } => None,
            Self::Target { backend_ref, .. } => Some(backend_ref),
            Self::BackendDefault { backend_ref, .. } | Self::BackendExact { backend_ref, .. } => {
                Some(backend_ref.backend_ref())
            }
            Self::Pinned(binding) => Some(&binding.backend_ref),
        }
    }

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

    pub fn try_backend_default(
        backend_ref: impl Into<String>,
        configuration: AcpSessionConfiguration,
    ) -> Result<Self, String> {
        Ok(Self::BackendDefault {
            backend_ref: AcpBackend::parse(backend_ref).map_err(|error| error.to_string())?,
            configuration,
        })
    }

    pub fn try_backend_exact(
        backend_ref: impl Into<String>,
        model_ref: impl Into<String>,
        configuration: AcpSessionConfiguration,
    ) -> Result<Self, String> {
        Ok(Self::BackendExact {
            backend_ref: AcpBackend::parse(backend_ref).map_err(|error| error.to_string())?,
            model_ref: ExactModelRef::parse(model_ref).map_err(str::to_string)?,
            configuration,
        })
    }

    /// The concrete binding if pinned. Policy selections return `None` because
    /// publication must resolve them before compile.
    #[must_use]
    pub fn resolved(&self) -> Option<&ModelBinding> {
        match self {
            ModelSelection::Pinned(binding) => Some(binding),
            ModelSelection::Auto
            | ModelSelection::Profile { .. }
            | ModelSelection::Target { .. }
            | ModelSelection::BackendDefault { .. }
            | ModelSelection::BackendExact { .. } => None,
        }
    }

    /// Whether the selection is still `Auto` (the reconciler re-resolves these).
    #[must_use]
    pub fn is_auto(&self) -> bool {
        matches!(self, ModelSelection::Auto)
    }

    /// Whether an authoritative catalog, profile, or Worker observation change
    /// can alter publication readiness. Backend-owned reconciliation preserves
    /// the authored backend/configuration; it only refreshes its live pins.
    #[must_use]
    pub fn requires_reconciliation(&self) -> bool {
        matches!(
            self,
            Self::Auto
                | Self::Profile { .. }
                | Self::Target { .. }
                | Self::BackendDefault { .. }
                | Self::BackendExact { .. }
        )
    }

    /// The exact backend requested with backend-owned default-model policy.
    #[must_use]
    pub fn backend_default_ref(&self) -> Option<&str> {
        match self {
            Self::BackendDefault { backend_ref, .. } => Some(backend_ref.backend_ref()),
            Self::Auto
            | Self::Profile { .. }
            | Self::Target { .. }
            | Self::BackendExact { .. }
            | Self::Pinned(_) => None,
        }
    }

    /// The explicitly selected reusable inference profile.
    #[must_use]
    pub fn profile_ref(&self) -> Option<&str> {
        match self {
            Self::Profile { profile_id } => Some(profile_id),
            Self::Auto
            | Self::Target { .. }
            | Self::BackendDefault { .. }
            | Self::BackendExact { .. }
            | Self::Pinned(_) => None,
        }
    }

    /// The unresolved catalog target and requested executor.
    #[must_use]
    pub fn target(&self) -> Option<(&ModelTarget, &str)> {
        match self {
            Self::Target {
                target,
                backend_ref,
                ..
            } => Some((target, backend_ref)),
            _ => None,
        }
    }

    #[must_use]
    pub fn backend_exact(&self) -> Option<(&str, &str)> {
        match self {
            Self::BackendExact {
                backend_ref,
                model_ref,
                ..
            } => Some((backend_ref.backend_ref(), model_ref.as_str())),
            _ => None,
        }
    }

    #[must_use]
    pub fn acp_configuration(&self) -> Option<&AcpSessionConfiguration> {
        match self {
            Self::Target { configuration, .. }
            | Self::BackendDefault { configuration, .. }
            | Self::BackendExact { configuration, .. } => Some(configuration),
            _ => None,
        }
    }

    /// Attach adapter-native Session intent to the existing executor selection.
    /// The model/executor grammar remains the sole route authority; this method
    /// rejects native/A2A/pinned selections instead of creating a parallel ACP
    /// selector in the public protocol adapter.
    pub fn set_acp_configuration(
        &mut self,
        configuration: AcpSessionConfiguration,
    ) -> Result<(), &'static str> {
        let backend_ref = self
            .backend_ref()
            .ok_or("ACP configuration requires an explicit acp:<cli> model selection")?;
        if !matches!(
            awaken_runtime_contract::resolved::Backend::from_ref(backend_ref),
            awaken_runtime_contract::resolved::Backend::Acp(_)
        ) {
            return Err("ACP configuration requires an explicit acp:<cli> model selection");
        }
        match self {
            Self::Target {
                configuration: current,
                ..
            }
            | Self::BackendDefault {
                configuration: current,
                ..
            }
            | Self::BackendExact {
                configuration: current,
                ..
            } => {
                *current = configuration;
                Ok(())
            }
            Self::Auto | Self::Profile { .. } | Self::Pinned(_) => {
                Err("ACP configuration requires an unresolved ACP model selection")
            }
        }
    }
}

/// Typed authoring view of ADR-0057's executor axis.
///
/// This value is derived from `ModelSelection.backend_ref`; it is deliberately
/// neither serialized nor stored on [`AgentConfig`]. Runtime keeps its own
/// [`Backend`](awaken_runtime_contract::resolved::Backend) context form, and
/// this config form is an explicit projection from that one parser.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AgentKind {
    Native,
    Acp(awaken_runtime_contract::resolved::AcpBackend),
    A2a(awaken_runtime_contract::resolved::A2aBackend),
    Invalid(awaken_runtime_contract::resolved::InvalidBackendRef),
}

impl AgentKind {
    #[must_use]
    pub fn from_backend_ref(backend_ref: &str) -> Self {
        match awaken_runtime_contract::resolved::Backend::from_ref(backend_ref) {
            awaken_runtime_contract::resolved::Backend::Native => Self::Native,
            awaken_runtime_contract::resolved::Backend::Acp(backend) => Self::Acp(backend),
            awaken_runtime_contract::resolved::Backend::Remote(backend) => Self::A2a(backend),
            awaken_runtime_contract::resolved::Backend::Invalid(invalid) => Self::Invalid(invalid),
        }
    }

    /// Canonical runtime coordinate for this derived kind.
    #[must_use]
    pub fn backend_ref(&self) -> String {
        match self {
            Self::Native => "genai".into(),
            Self::Acp(backend) => format!("acp:{backend}"),
            Self::A2a(backend) => format!("a2a:{backend}"),
            Self::Invalid(invalid) => invalid.as_str().to_string(),
        }
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
            ModelSelection::Pinned(binding) => {
                use serde::ser::SerializeMap;
                let mut map = serializer.serialize_map(Some(4))?;
                map.serialize_entry("mode", "pinned")?;
                map.serialize_entry("provider_identity_ref", &binding.provider_identity_ref)?;
                map.serialize_entry("model_ref", &binding.model_ref)?;
                map.serialize_entry("backend_ref", &binding.backend_ref)?;
                map.end()
            }
            ModelSelection::Auto => {
                use serde::ser::SerializeMap;
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("mode", "auto")?;
                map.end()
            }
            ModelSelection::Profile { profile_id } => {
                use serde::ser::SerializeMap;
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("mode", "profile")?;
                map.serialize_entry("profile_id", profile_id)?;
                map.end()
            }
            ModelSelection::Target {
                target,
                backend_ref,
                configuration,
            } => {
                use serde::ser::SerializeMap;
                let mut map = serializer.serialize_map(Some(4))?;
                map.serialize_entry("mode", "target")?;
                map.serialize_entry("target", target)?;
                map.serialize_entry("backend_ref", backend_ref)?;
                if !configuration.is_empty() {
                    map.serialize_entry("configuration", configuration)?;
                }
                map.end()
            }
            ModelSelection::BackendDefault {
                backend_ref,
                configuration,
            } => {
                use serde::ser::SerializeMap;
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("mode", "backend_default")?;
                map.serialize_entry("backend_ref", backend_ref.backend_ref())?;
                if !configuration.is_empty() {
                    map.serialize_entry("configuration", configuration)?;
                }
                map.end()
            }
            ModelSelection::BackendExact {
                backend_ref,
                model_ref,
                configuration,
            } => {
                use serde::ser::SerializeMap;
                let mut map = serializer.serialize_map(Some(4))?;
                map.serialize_entry("mode", "backend_exact")?;
                map.serialize_entry("backend_ref", backend_ref.backend_ref())?;
                map.serialize_entry("model_ref", model_ref.as_str())?;
                if !configuration.is_empty() {
                    map.serialize_entry("configuration", configuration)?;
                }
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for ModelSelection {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match ModelSelectionWire::deserialize(deserializer)? {
            ModelSelectionWire::Auto => Ok(Self::Auto),
            ModelSelectionWire::Profile { profile_id } if !profile_id.trim().is_empty() => {
                Ok(Self::Profile { profile_id })
            }
            ModelSelectionWire::Profile { .. } => {
                Err(serde::de::Error::custom("profile_id must not be empty"))
            }
            ModelSelectionWire::Target {
                target,
                backend_ref,
                configuration,
            } if !(target.model_id.trim().is_empty()
                || backend_ref.trim().is_empty()
                || target.protocol_endpoint_id.is_some() && target.endpoint_name.is_some()) =>
            {
                Ok(Self::Target {
                    target,
                    backend_ref,
                    configuration,
                })
            }
            ModelSelectionWire::Target { .. } => Err(serde::de::Error::custom(
                "target requires model_id and backend_ref and cannot combine endpoint_name with protocol_endpoint_id",
            )),
            ModelSelectionWire::BackendDefault {
                backend_ref,
                configuration,
            } => Self::try_backend_default(backend_ref, configuration)
                .map_err(serde::de::Error::custom),
            ModelSelectionWire::BackendExact {
                backend_ref,
                model_ref,
                configuration,
            } => Self::try_backend_exact(backend_ref, model_ref, configuration)
                .map_err(serde::de::Error::custom),
            ModelSelectionWire::Pinned {
                provider_identity_ref,
                model_ref,
                backend_ref,
            } => Ok(Self::pinned(provider_identity_ref, model_ref, backend_ref)),
        }
    }
}

#[derive(Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
enum ModelSelectionWire {
    Auto,
    Profile {
        profile_id: String,
    },
    Target {
        target: ModelTarget,
        backend_ref: String,
        #[serde(default)]
        configuration: AcpSessionConfiguration,
    },
    BackendDefault {
        backend_ref: String,
        #[serde(default)]
        configuration: AcpSessionConfiguration,
    },
    BackendExact {
        backend_ref: String,
        model_ref: String,
        #[serde(default)]
        configuration: AcpSessionConfiguration,
    },
    Pinned {
        provider_identity_ref: String,
        model_ref: String,
        backend_ref: String,
    },
}

#[cfg(feature = "schema")]
impl schemars::JsonSchema for ModelSelection {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ModelSelection".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        ModelSelectionWire::json_schema(generator)
    }
}

/// One authored coordinator-roster target. This is the config domain's one
/// canonical representation of the public string / versioned-agent / `self`
/// union; execution resolves it to Agent ids during compilation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MultiagentTarget {
    Agent { id: String, version: Option<u64> },
    SelfReference,
    Advisor { model: String },
}

impl MultiagentTarget {
    #[must_use]
    pub fn resolved_id<'a>(&'a self, owner_id: &'a str) -> &'a str {
        match self {
            Self::Agent { id, .. } => id,
            Self::SelfReference => owner_id,
            Self::Advisor { .. } => "anthropic.advisor",
        }
    }

    #[must_use]
    pub fn version(&self) -> Option<u64> {
        match self {
            Self::Agent { version, .. } => *version,
            Self::SelfReference => None,
            Self::Advisor { .. } => None,
        }
    }

    #[must_use]
    pub fn is_self_reference(&self) -> bool {
        matches!(self, Self::SelfReference)
    }

    #[must_use]
    pub fn advisor_model(&self) -> Option<&str> {
        match self {
            Self::Advisor { model } => Some(model),
            _ => None,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum MultiagentTargetWire {
    Id(String),
    Tagged(MultiagentTaggedTarget),
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum MultiagentTaggedTarget {
    Agent {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<u64>,
    },
    #[serde(rename = "self")]
    SelfReference,
    Advisor {
        model: String,
    },
}

impl Serialize for MultiagentTarget {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let wire = match self {
            Self::Agent { id, version: None } => MultiagentTargetWire::Id(id.clone()),
            Self::Agent { id, version } => {
                MultiagentTargetWire::Tagged(MultiagentTaggedTarget::Agent {
                    id: id.clone(),
                    version: *version,
                })
            }
            Self::SelfReference => {
                MultiagentTargetWire::Tagged(MultiagentTaggedTarget::SelfReference)
            }
            Self::Advisor { model } => {
                MultiagentTargetWire::Tagged(MultiagentTaggedTarget::Advisor {
                    model: model.clone(),
                })
            }
        };
        wire.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for MultiagentTarget {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match MultiagentTargetWire::deserialize(deserializer)? {
            MultiagentTargetWire::Id(id) => Self::Agent { id, version: None },
            MultiagentTargetWire::Tagged(MultiagentTaggedTarget::Agent { id, version }) => {
                Self::Agent { id, version }
            }
            MultiagentTargetWire::Tagged(MultiagentTaggedTarget::SelfReference) => {
                Self::SelfReference
            }
            MultiagentTargetWire::Tagged(MultiagentTaggedTarget::Advisor { model }) => {
                Self::Advisor { model }
            }
        })
    }
}

/// The delegation roster authored for one Agent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MultiagentConfig {
    pub agents: Vec<MultiagentTarget>,
}

impl MultiagentConfig {
    /// Validate the roster's representation-independent invariants once. Managed
    /// admission and publication compilation both call this owner so malformed
    /// generic-config writes cannot bypass the HTTP edge without duplicating the
    /// rules in two bounded contexts.
    pub fn validate(&self, owner_id: &str) -> Result<(), String> {
        if self.agents.is_empty() {
            return Err("agents must contain at least one entry".into());
        }
        let ordinary_count = self
            .agents
            .iter()
            .filter(|target| target.advisor_model().is_none())
            .count();
        if ordinary_count > 20 {
            return Err("agents supports at most 20 ordinary Agent entries".into());
        }
        let mut saw_self = false;
        let mut saw_advisor = false;
        let mut seen = std::collections::BTreeSet::new();
        for (index, target) in self.agents.iter().enumerate() {
            let id = target.resolved_id(owner_id).trim();
            if id.is_empty() {
                return Err(format!("entry {index} must be a non-empty Agent id"));
            }
            if target.is_self_reference() {
                if saw_self {
                    return Err("at most one `self` entry is allowed".into());
                }
                saw_self = true;
            } else if let Some(model) = target.advisor_model() {
                if saw_advisor {
                    return Err("at most one `advisor` entry is allowed".into());
                }
                if model.trim().is_empty() {
                    return Err(format!("entry {index} advisor model must be non-empty"));
                }
                saw_advisor = true;
            } else if id == owner_id {
                return Err("recursive invocation must use the `self` roster entry".into());
            } else if id == "anthropic.advisor" {
                return Err("`anthropic.advisor` is reserved for the advisor entry".into());
            }
            if target.version() == Some(0) {
                return Err(format!("entry {index} version must be at least 1"));
            }
            if !seen.insert(id.to_string()) {
                return Err(format!("Agent id {id:?} is duplicated"));
            }
        }
        Ok(())
    }
}

impl Serialize for MultiagentConfig {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct as _;

        let mut state = serializer.serialize_struct("MultiagentConfig", 2)?;
        state.serialize_field("type", "coordinator")?;
        state.serialize_field("agents", &self.agents)?;
        state.end()
    }
}

#[derive(Deserialize)]
struct MultiagentConfigWire {
    #[serde(rename = "type")]
    kind: String,
    agents: Vec<MultiagentTarget>,
}

impl<'de> Deserialize<'de> for MultiagentConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = MultiagentConfigWire::deserialize(deserializer)?;
        if wire.kind != "coordinator" {
            return Err(serde::de::Error::custom(
                "multiagent.type must be `coordinator`",
            ));
        }
        Ok(Self {
            agents: wire.agents,
        })
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
    /// The model selection (ADR-0052 D5): automatic discovery, an explicit
    /// profile, a backend-owned default, or one concrete pinned binding.
    pub model_binding: ModelSelection,
    /// Provider-neutral call controls authored with the model and frozen into
    /// every executable revision. They are not part of model route identity.
    #[serde(default, skip_serializing_if = "InferenceOptions::is_default")]
    pub inference: InferenceOptions,
    pub tool_ids: Vec<String>,
    /// Typed tool-family policy authored by protocol adapters. Static and MCP
    /// execution both compile from this one source; empty preserves legacy exact
    /// `tool_ids` behavior.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub toolsets: Vec<ToolsetPolicy>,
    /// Inline client-executed tools. These are capabilities advertised to the
    /// model, not aliases for host catalog tools with the same name. Their exact
    /// description and schema are part of the Agent revision and publication
    /// fingerprint.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub client_tools: Vec<ToolDescriptor>,
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
    #[serde(
        default,
        rename = "model_candidates",
        alias = "model_fallbacks",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub model_fallbacks: Vec<ModelBinding>,
    /// Managed-Agent identity fields, carried so the config plane's agent object
    /// stays consistent with the SDK `/v1/agents` object. Identity metadata is
    /// excluded from the behavioral fingerprint; the typed MCP/Skill/delegation
    /// bindings below are compiled into the executable snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<awaken_runtime_contract::agent_bindings::AgentMcpServerBinding>,
    /// Typed Skill resources selected by this authoring revision. Legacy string
    /// ids deserialize as `custom/latest`; serialization always emits one exact
    /// official object form.
    #[serde(
        default,
        rename = "skills",
        alias = "skill_ids",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub skills: Vec<awaken_agent_contract::AgentSkillBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multiagent: Option<MultiagentConfig>,
    /// Time at which new execution was disabled. Disabled Agents remain
    /// readable and retain their immutable publications, but cannot be selected
    /// for new execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_at: Option<String>,
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

/// The one lifecycle projection of an Agent authoring aggregate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentLifecycle {
    Published,
    Disabled,
    Archived,
}

impl AgentConfig {
    #[must_use]
    pub fn lifecycle(&self) -> AgentLifecycle {
        if self.archived_at.is_some() {
            AgentLifecycle::Archived
        } else if self.disabled_at.is_some() {
            AgentLifecycle::Disabled
        } else {
            AgentLifecycle::Published
        }
    }
}

impl AgentConfig {
    /// Project the executor kind without adding a parallel persisted
    /// discriminant. Unresolved Auto/Profile selections belong to the native
    /// resolver; every explicit selection is parsed by the canonical runtime
    /// backend vocabulary.
    #[must_use]
    pub fn kind(&self) -> AgentKind {
        self.model_binding
            .backend_ref()
            .map(AgentKind::from_backend_ref)
            .unwrap_or(AgentKind::Native)
    }
}

fn delegation_limits_are_default(limits: &DelegationLimits) -> bool {
    limits == &DelegationLimits::default()
}

#[cfg(test)]
mod model_selection_tests {
    use super::{AgentConfig, AgentKind, ModelSelection};

    #[test]
    fn profile_is_explicit_and_catalog_reconciled() {
        // | Choice | profile_ref | requires reconciliation |
        // | Auto | none | yes |
        // | Profile | exact id | yes |
        // | BackendDefault/Exact | none | yes (live Worker observation) |
        // | Pinned | none | no |
        let selection = ModelSelection::Profile {
            profile_id: "latency-route".into(),
        };
        assert_eq!(selection.profile_ref(), Some("latency-route"));
        assert!(selection.requires_reconciliation());
        assert_eq!(
            serde_json::to_value(&selection).unwrap(),
            serde_json::json!({"mode":"profile","profile_id":"latency-route"})
        );
        assert_eq!(
            serde_json::from_value::<ModelSelection>(
                serde_json::json!({"mode":"profile","profile_id":"latency-route"})
            )
            .unwrap(),
            selection
        );
        assert!(ModelSelection::Auto.requires_reconciliation());
        assert!(
            ModelSelection::try_backend_default("acp:codex", Default::default())
                .expect("exact ACP backend")
                .requires_reconciliation()
        );
        assert!(
            ModelSelection::try_backend_exact("acp:codex", "gpt-exact", Default::default())
                .expect("exact ACP selection")
                .requires_reconciliation()
        );
        assert!(!ModelSelection::pinned("provider", "model", "genai").requires_reconciliation());
    }

    #[test]
    fn backend_owned_selection_cannot_encode_an_inexact_backend_or_model() {
        // Boundary-value partitions: Default requires an exact ACP executor;
        // Exact additionally requires one non-blank, already-canonical model.
        // Both Rust constructors and the wire decoder share those same types.
        for backend_ref in ["", "acp", "acp:", "genai", " acp:codex", "acp:codex "] {
            assert!(
                ModelSelection::try_backend_default(backend_ref, Default::default()).is_err(),
                "rejected backend {backend_ref:?}"
            );
        }
        for model_ref in ["", " ", " gpt-exact", "gpt-exact "] {
            assert!(
                ModelSelection::try_backend_exact("acp:codex", model_ref, Default::default())
                    .is_err(),
                "rejected model {model_ref:?}"
            );
        }
        for wire in [
            serde_json::json!({"mode":"backend_default","backend_ref":"genai"}),
            serde_json::json!({"mode":"backend_default","backend_ref":"acp:"}),
            serde_json::json!({"mode":"backend_exact","backend_ref":"acp:codex","model_ref":""}),
            serde_json::json!({"mode":"backend_exact","backend_ref":"acp:codex","model_ref":" model"}),
        ] {
            assert!(serde_json::from_value::<ModelSelection>(wire).is_err());
        }
    }

    // Cause/effect decision table for the derived executor lens:
    // R1 Auto/Profile -> Native; R2 ordinary provider ref -> Native;
    // R3 exact acp:<cli> -> Acp; R4 exact a2a:<endpoint> -> A2a; R5 malformed
    // executor coordinates -> Invalid. Effects: no serialized `kind` field and the
    // canonical encoded coordinate parses back to the same discriminant.
    #[test]
    fn agent_kind_is_a_stable_derived_executor_view() {
        for backend_ref in [
            "genai",
            "provider:custom",
            "acp:codex",
            "a2a:https://agent.example",
        ] {
            let config = AgentConfig {
                model_binding: ModelSelection::pinned("provider", "model", backend_ref),
                ..Default::default()
            };
            let expected = config.kind();
            assert_eq!(
                AgentKind::from_backend_ref(&config.kind().backend_ref()),
                expected
            );
            assert!(
                serde_json::to_value(&config)
                    .expect("serialize config")
                    .get("kind")
                    .is_none(),
                "kind remains a projection, never stored twice"
            );
        }
        assert!(matches!(
            AgentKind::from_backend_ref("acp"),
            AgentKind::Invalid(_)
        ));
        assert!(matches!(
            AgentKind::from_backend_ref("a2a:"),
            AgentKind::Invalid(_)
        ));
        for selection in [
            ModelSelection::Auto,
            ModelSelection::Profile {
                profile_id: "default".into(),
            },
        ] {
            assert_eq!(
                AgentConfig {
                    model_binding: selection,
                    ..Default::default()
                }
                .kind(),
                AgentKind::Native
            );
        }
    }

    // Cause/effect decision table for the model-fallback terminology migration:
    // R1 historical `model_candidates` input -> populate model_fallbacks;
    // R2 preferred `model_fallbacks` input -> populate the same field;
    // R3 either input -> serialize the historical fingerprint key only.
    #[test]
    fn model_fallbacks_accept_both_authoring_names_without_wire_drift() {
        let fallback = serde_json::json!([{
            "provider_identity_ref": "provider",
            "model_ref": "fallback",
            "backend_ref": "genai"
        }]);
        for input_key in ["model_candidates", "model_fallbacks"] {
            let mut value = serde_json::to_value(AgentConfig::default()).unwrap();
            value
                .as_object_mut()
                .unwrap()
                .insert(input_key.into(), fallback.clone());
            let config: AgentConfig = serde_json::from_value(value).unwrap();
            assert_eq!(config.model_fallbacks.len(), 1, "{input_key}");
            let encoded = serde_json::to_value(config).unwrap();
            assert_eq!(encoded["model_candidates"], fallback, "{input_key}");
            assert!(encoded.get("model_fallbacks").is_none(), "{input_key}");
        }
    }

    #[test]
    fn empty_profile_id_is_rejected() {
        let error = serde_json::from_value::<ModelSelection>(
            serde_json::json!({"mode":"profile","profile_id":" "}),
        )
        .expect_err("blank profile id");
        assert!(error.to_string().contains("profile_id"));
    }
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
