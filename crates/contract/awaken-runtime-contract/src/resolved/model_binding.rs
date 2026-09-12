//! The model binding: the exact provider identity, model, and backend one
//! attempt runs against, and the one projection every comparison of two
//! bindings goes through.
//!
//! Equality here is a decision — publication deduplicates candidates and the
//! brokered-inference grant is issued only on an exact match — so both halves
//! of that decision are stated in this module: which coordinates a producer
//! must supply, and which coordinates the comparison reads.

use serde::{Deserialize, Serialize};

use super::Backend;

/// The three coordinates that jointly identify one model binding, preserved as
/// a tuple. This is the *only* projection production code uses to compare two
/// bindings, so "which coordinates are part of this identity?" has exactly one
/// answer and a later field cannot silently join or leave the comparison.
/// Nothing here parses or concatenates a coordinate with another.
fn model_binding_identity<T>(provider_identity_ref: T, model_ref: T, backend_ref: T) -> (T, T, T) {
    (provider_identity_ref, model_ref, backend_ref)
}

/// Why a set of coordinates is not an executable model binding.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidModelBinding {
    #[error("model binding backend reference `{0}` is not an executable backend")]
    Backend(String),
    #[error("model binding {coordinate} `{value}` carries surrounding whitespace")]
    Whitespace {
        coordinate: &'static str,
        value: String,
    },
    #[error(
        "a native model binding must state both a provider identity and a model; \
         got provider identity `{provider_identity_ref}` and model `{model_ref}`"
    )]
    IncompleteNative {
        provider_identity_ref: String,
        model_ref: String,
    },
}

/// Proof that the three coordinates beside it were admitted together by
/// [`ModelBinding::parse`]. Its privacy is the seal: a `ModelBinding` literal
/// cannot be written outside this module, so the validating constructor is the
/// only way to obtain one and an under-specified binding is unrepresentable.
#[derive(Debug, Clone, Copy)]
struct BindingCoherence;

/// The exact provider identity, model, and backend one attempt runs against.
///
/// All three coordinates are significant for equality, and equality is a
/// decision: publication deduplicates candidates in a `BTreeSet<ModelBinding>`
/// and the brokered-inference grant is issued only when the operation's binding
/// equals the requested one. A binding that leaves a coordinate blank therefore
/// compares equal to a binding on a *different* provider account, which is how
/// a pinned model once resolved to the same model name on the wrong provider
/// and billed the wrong account. Emptiness is licensed by the backend kind, and
/// nothing else may leave a coordinate blank.
///
/// The struct is sealed; a literal is a compile error:
///
/// ```compile_fail
/// use awaken_runtime_contract::resolved::ModelBinding;
///
/// let _ = ModelBinding {
///     provider_identity_ref: String::new(),
///     model_ref: "m-first".to_string(),
///     backend_ref: String::new(),
/// };
/// ```
#[derive(Clone, Serialize, Deserialize)]
#[serde(try_from = "ModelBindingWire")]
pub struct ModelBinding {
    /// The *provider identity* — the principal whose key and quota this attempt
    /// runs under. It is the cooldown / account-spread key: two candidates on the
    /// same model but different identities differ here, so each tracks its own
    /// circuit and quota. (Formerly `provider_instance_ref`; renamed to name the
    /// principal, aligning with awaken-next's `ProviderIdentity`.)
    pub provider_identity_ref: String,
    pub model_ref: String,
    pub backend_ref: String,
    #[serde(skip)]
    #[allow(
        dead_code,
        reason = "the seal is the field's privacy, not its value: it makes a \
                  ModelBinding literal unwritable outside this module"
    )]
    coherence: BindingCoherence,
}

/// The durable three-coordinate wire shape. Deserialization goes through
/// [`ModelBinding::parse`] like every other producer, so a snapshot written
/// before the completeness rule cannot restore an under-specified binding into
/// a comparison fence; it fails closed instead.
#[derive(Deserialize)]
struct ModelBindingWire {
    provider_identity_ref: String,
    model_ref: String,
    backend_ref: String,
}

impl TryFrom<ModelBindingWire> for ModelBinding {
    type Error = InvalidModelBinding;

    fn try_from(wire: ModelBindingWire) -> Result<Self, Self::Error> {
        Self::parse(wire.provider_identity_ref, wire.model_ref, wire.backend_ref)
    }
}

impl std::fmt::Debug for ModelBinding {
    /// The coherence proof is an invariant, not a coordinate; printing it would
    /// put a second spelling of the identity into operator-facing messages.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ModelBinding")
            .field("provider_identity_ref", &self.provider_identity_ref)
            .field("model_ref", &self.model_ref)
            .field("backend_ref", &self.backend_ref)
            .finish()
    }
}

impl PartialEq for ModelBinding {
    fn eq(&self, other: &Self) -> bool {
        self.identity() == other.identity()
    }
}

impl Eq for ModelBinding {}

impl PartialOrd for ModelBinding {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ModelBinding {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.identity().cmp(&other.identity())
    }
}

impl std::hash::Hash for ModelBinding {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.identity().hash(state);
    }
}

/// A model coordinate proven non-empty and free of accidental surrounding
/// whitespace. The private payload prevents an `Exact` policy from carrying no
/// exact model at all.
///
/// ```compile_fail
/// use awaken_runtime_contract::resolved::ExactModelRef;
/// let _ = ExactModelRef(String::new());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ExactModelRef(String);

impl ExactModelRef {
    pub fn parse(model_ref: impl Into<String>) -> Result<Self, &'static str> {
        let model_ref = model_ref.into();
        if model_ref.is_empty() || model_ref.trim() != model_ref {
            return Err(
                "exact model reference must be non-empty and contain no surrounding whitespace",
            );
        }
        Ok(Self(model_ref))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ExactModelRef {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The principal whose key and quota one attempt runs under, proven non-empty
/// and free of accidental surrounding whitespace. The private payload prevents
/// a blank account coordinate from entering a comparison fence, where it would
/// make two candidates on *different* provider accounts compare equal.
///
/// ```compile_fail
/// use awaken_runtime_contract::resolved::ProviderIdentityRef;
/// let _ = ProviderIdentityRef(String::new());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProviderIdentityRef(String);

impl ProviderIdentityRef {
    pub fn parse(provider_identity_ref: impl Into<String>) -> Result<Self, &'static str> {
        let provider_identity_ref = provider_identity_ref.into();
        if provider_identity_ref.is_empty() || provider_identity_ref.trim() != provider_identity_ref
        {
            return Err(
                "provider identity reference must be non-empty and contain no surrounding whitespace",
            );
        }
        Ok(Self(provider_identity_ref))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ProviderIdentityRef {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl ModelBinding {
    /// Admit one complete set of coordinates as an executable model binding.
    ///
    /// This is the only constructor. The rules are exactly the ones the
    /// comparison fences depend on:
    ///
    /// * `backend_ref` must name an executable backend — [`Backend::Invalid`]
    ///   is a parse outcome, never a route, so it can never enter a binding.
    /// * no coordinate may carry surrounding whitespace, so two spellings of
    ///   one coordinate cannot compare unequal.
    /// * a **native** backend must state both a provider identity and a model.
    ///   An external backend owns its own account and may legitimately leave
    ///   either blank (an ACP executor keeps its default model; an A2A endpoint
    ///   *is* the identity), but a native route without a provider identity has
    ///   no account coordinate at all and would compare equal to a binding on a
    ///   different provider.
    pub fn parse(
        provider_identity_ref: impl Into<String>,
        model_ref: impl Into<String>,
        backend_ref: impl Into<String>,
    ) -> Result<Self, InvalidModelBinding> {
        let (provider_identity_ref, model_ref, backend_ref) = model_binding_identity(
            provider_identity_ref.into(),
            model_ref.into(),
            backend_ref.into(),
        );
        for (coordinate, value) in [
            ("provider identity", &provider_identity_ref),
            ("model", &model_ref),
            ("backend", &backend_ref),
        ] {
            if value.trim() != value {
                return Err(InvalidModelBinding::Whitespace {
                    coordinate,
                    value: value.clone(),
                });
            }
        }
        match Backend::from_ref(&backend_ref) {
            Backend::Invalid(_) => return Err(InvalidModelBinding::Backend(backend_ref)),
            Backend::Native if provider_identity_ref.is_empty() || model_ref.is_empty() => {
                return Err(InvalidModelBinding::IncompleteNative {
                    provider_identity_ref,
                    model_ref,
                });
            }
            Backend::Native | Backend::Acp(_) | Backend::Remote(_) => {}
        }
        Ok(Self {
            provider_identity_ref,
            model_ref,
            backend_ref,
            coherence: BindingCoherence,
        })
    }

    /// Construct a binding **without** the completeness check.
    ///
    /// This is the pre-existing producer path, kept only so the call sites that
    /// predate [`ModelBinding::parse`] keep compiling. It is not a second
    /// contract: `scripts/ci/check_crate_boundaries.py` ratchets the number of
    /// its call sites, so the count may fall and may not rise. Every new
    /// producer uses `parse`, and the durable path already does — deserializing
    /// a binding goes through `parse` and fails closed.
    #[must_use]
    pub fn new(
        provider_identity_ref: impl Into<String>,
        model_ref: impl Into<String>,
        backend_ref: impl Into<String>,
    ) -> Self {
        let (provider_identity_ref, model_ref, backend_ref) = model_binding_identity(
            provider_identity_ref.into(),
            model_ref.into(),
            backend_ref.into(),
        );
        Self {
            provider_identity_ref,
            model_ref,
            backend_ref,
            coherence: BindingCoherence,
        }
    }

    /// Borrow the exact tuple every comparison fence compares.
    #[must_use]
    pub fn identity(&self) -> (&str, &str, &str) {
        model_binding_identity(
            &self.provider_identity_ref,
            &self.model_ref,
            &self.backend_ref,
        )
    }

    /// The proven provider identity, or `None` when the backend owns the
    /// account. Absence is a state of its own here, never the empty string.
    #[must_use]
    pub fn provider_identity(&self) -> Option<ProviderIdentityRef> {
        ProviderIdentityRef::parse(self.provider_identity_ref.clone()).ok()
    }

    /// The proven exact model, or `None` when the backend keeps its default.
    #[must_use]
    pub fn model(&self) -> Option<ExactModelRef> {
        ExactModelRef::parse(self.model_ref.clone()).ok()
    }

    /// The executable backend. `parse` already refused [`Backend::Invalid`].
    #[must_use]
    pub fn backend(&self) -> Backend {
        Backend::from_ref(&self.backend_ref)
    }
}
