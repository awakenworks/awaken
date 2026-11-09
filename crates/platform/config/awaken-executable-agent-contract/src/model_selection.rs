// @formal surface requirement=control.agent_profiles.immutable_profile
// @formal reason: The Config-domain authoring selection value, validating serde boundary and
// @formal reason: constructors are shared by authoring and publication projections. This owns
// @formal reason: no registration, catalog, credential or runtime state.
//! The one pure Config-domain model-selection value and wire admission.
use awaken_agent_contract::{AcpSessionConfiguration, ModelTarget};
use awaken_runtime_contract::resolved::{
    AcpBackend, ExactModelRef, InvalidModelBinding, ModelBinding,
};
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
            Self::Pinned(binding) => Some(binding.identity().2),
        }
    }

    /// A pinned binding from its three refs. The coordinates are admitted by
    /// [`ModelBinding::parse`], so a selection that names a non-executable
    /// backend or an under-specified native route is refused here rather than
    /// at the durable boundary.
    pub fn pinned(
        provider_identity_ref: impl Into<String>,
        model_ref: impl Into<String>,
        backend_ref: impl Into<String>,
    ) -> Result<Self, InvalidModelBinding> {
        ModelBinding::parse(provider_identity_ref, model_ref, backend_ref)
            .map(ModelSelection::Pinned)
    }

    pub fn try_backend_default(
        backend_ref: impl Into<String>,
        configuration: AcpSessionConfiguration,
    ) -> Result<Self, String> {
        configuration
            .validate_working_directory()
            .map_err(str::to_string)?;
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
        configuration
            .validate_working_directory()
            .map_err(str::to_string)?;
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
        configuration.validate_working_directory()?;
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
                map.serialize_entry("provider_identity_ref", &binding.identity().0)?;
                map.serialize_entry("model_ref", &binding.identity().1)?;
                map.serialize_entry("backend_ref", &binding.identity().2)?;
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
            ModelSelectionWire::Auto {} => Ok(Self::Auto),
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
                configuration
                    .validate_working_directory()
                    .map_err(serde::de::Error::custom)?;
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
            } => Self::pinned(provider_identity_ref, model_ref, backend_ref)
                .map_err(serde::de::Error::custom),
        }
    }
}

#[derive(Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
enum ModelSelectionWire {
    // Spelled as an empty struct variant, not a unit variant, deliberately:
    // serde's `deny_unknown_fields` does not reach the unit variants of an
    // internally tagged enum, so `{"mode":"auto","backend_ref":"acp:codex"}`
    // would deserialize as plain `Auto` with the authored backend silently
    // discarded. An empty struct variant is denied the same way every other
    // mode is, so no authored field can be dropped without being reported.
    Auto {},
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
