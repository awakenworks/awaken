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
            // Half a native binding is the incoherence worth refusing: a
            // provider with no model, or a model with no provider, names
            // nothing executable. Both empty is a different shape entirely --
            // a host-installed executor (`ResolvedModelCandidate::host`) names
            // itself in `backend_ref` and has no provider or model to state --
            // and refusing it made every such binding constructible but not
            // decodable, which stalled runs at the wire instead of failing
            // where the coordinates were written.
            Backend::Native if provider_identity_ref.is_empty() != model_ref.is_empty() => {
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

    /// Construct a binding from coordinates the caller asserts are complete.
    ///
    /// This applies exactly the admission [`ModelBinding::parse`] applies and
    /// panics where `parse` would refuse. There is one rule set, so a binding
    /// that exists in memory is always one the wire decodes: the alternative
    /// -- a permissive constructor beside a strict decoder -- let a value be
    /// built here and refused at the durable boundary, which stalled a run
    /// silently instead of failing loudly where the coordinates were written.
    ///
    /// A panic here is a programming error at the call site (the coordinates
    /// are literals or already-validated values), never an input-validation
    /// path: code that admits untrusted coordinates uses `parse` and returns
    /// the [`InvalidModelBinding`]. `scripts/ci/check_crate_boundaries.py`
    /// ratchets the number of `new` call sites so the migration to `parse`
    /// only moves forward.
    #[must_use]
    #[track_caller]
    pub fn new(
        provider_identity_ref: impl Into<String>,
        model_ref: impl Into<String>,
        backend_ref: impl Into<String>,
    ) -> Self {
        match Self::parse(provider_identity_ref, model_ref, backend_ref) {
            Ok(binding) => binding,
            Err(refusal) => {
                panic!("ModelBinding::new was handed coordinates the wire refuses: {refusal}")
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether `new` accepts the coordinates, observed without unwinding the
    /// test: a panic is the refusal.
    fn constructible(provider: &str, model: &str, backend: &str) -> bool {
        let provider = provider.to_string();
        let model = model.to_string();
        let backend = backend.to_string();
        std::panic::catch_unwind(move || ModelBinding::new(provider, model, backend)).is_ok()
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig {
            cases: 512,
            ..proptest::prelude::ProptestConfig::default()
        })]

        /// Property: the constructor and the wire agree on every input. C1
        /// `parse` admits the coordinates; C2 `new` builds them. Effects: E1
        /// both admit and the wire round-trips the built value exactly; E2
        /// both refuse. Rule R1 = C1 <=> C2. A third outcome -- built but not
        /// decodable, or decodable but not buildable -- is the silent-stall
        /// class this property exists to make unrepresentable.
        #[test]
        fn every_constructible_binding_round_trips_through_the_wire(
            provider in proptest::string::string_regex("[a-z ]{0,6}").unwrap(),
            model in proptest::string::string_regex("[a-z ]{0,6}").unwrap(),
            backend in proptest::sample::select(vec![
                "", " ", "genai", "scripted", " genai", "acp", "acp:", "acp:claude",
                "acp: x", "a2a:https://agent.example", "a2a:not a url", "a2a:",
            ]),
        ) {
            let parsed = ModelBinding::parse(provider.clone(), model.clone(), backend);
            let built = constructible(&provider, &model, backend);
            proptest::prop_assert_eq!(parsed.is_ok(), built, "R1: new and parse must agree");
            if let Ok(binding) = parsed {
                let wire = serde_json::to_value(&binding).expect("a built binding serializes");
                let decoded: ModelBinding =
                    serde_json::from_value(wire).expect("E1: the wire decodes what new built");
                proptest::prop_assert_eq!(decoded, binding);
            }
        }
    }

    #[test]
    fn the_documented_shapes_are_admitted_or_refused_exactly() {
        // The named rows of the rule the property above quantifies over:
        // T1 host-installed executor (both empty, native ref); T2 complete
        // native; T3 ACP with a model default; T4 A2A endpoint as identity;
        // T5/T6 half a native binding; T7 an inexecutable backend ref; T8 a
        // padded coordinate.
        for (provider, model, backend) in [
            ("", "", "scripted"),
            ("", "", "native"),
            ("anthropic", "claude-opus-4", "native"),
            ("scenario", "model", "acp:claude"),
            ("", "", "a2a:https://agent.example"),
        ] {
            let built = ModelBinding::new(provider, model, backend);
            let wire = serde_json::to_value(&built).expect("serializes");
            let decoded: ModelBinding = serde_json::from_value(wire).expect("T1-T4 decode");
            assert_eq!(decoded, built);
        }
        assert!(
            matches!(
                ModelBinding::parse("anthropic", "", "native"),
                Err(InvalidModelBinding::IncompleteNative { .. })
            ),
            "T5"
        );
        assert!(
            matches!(
                ModelBinding::parse("", "claude-opus-4", "native"),
                Err(InvalidModelBinding::IncompleteNative { .. })
            ),
            "T6"
        );
        assert!(
            matches!(
                ModelBinding::parse("p", "m", ""),
                Err(InvalidModelBinding::Backend(_))
            ),
            "T7"
        );
        assert!(
            matches!(
                ModelBinding::parse(" p", "m", "native"),
                Err(InvalidModelBinding::Whitespace { .. })
            ),
            "T8"
        );
        assert!(
            !constructible("anthropic", "", "native"),
            "T5 is not constructible either"
        );
        assert!(
            !constructible("p", "m", ""),
            "T7 is not constructible either"
        );
    }
}

/// Kani exhausts the admission rule over every coordinate *shape*: the rule
/// depends only on whether each coordinate is empty, padded, or plain, and on
/// the backend class, so a representative string per shape is a complete
/// enumeration of the decision. The two effects are E1 admit and E2 refuse.
///
/// | Rule | provider | model   | backend        | Effect |
/// | A1   | plain    | plain   | native         | E1     |
/// | A2   | empty    | empty   | native         | E1 (host-installed executor) |
/// | A3   | any      | any     | acp:/a2a: ok   | E1 unless padded |
/// | A4   | empty    | plain   | native         | E2 IncompleteNative |
/// | A5   | plain    | empty   | native         | E2 IncompleteNative |
/// | A6   | any      | any     | invalid        | E2 Backend |
/// | A7   | padded   | any     | any            | E2 Whitespace |
#[cfg(kani)]
#[kani::proof]
fn model_binding_admission_is_exactly_the_documented_rule_table() {
    // Every coordinate shape crossed with every backend class, as concrete
    // strings: the rule depends only on shape and class, and concrete strings
    // keep the byte loops inside `trim`/`strip_prefix` bounded so CBMC
    // exhausts all 54 rows instead of unwinding a symbolic pointer.
    const COORDINATES: [&str; 3] = ["", "plain", " padded"];
    const BACKENDS: [(&str, bool, bool); 6] = [
        // (reference, is native, is an inexecutable reference)
        ("native", true, false),
        ("acp:claude", false, false),
        ("a2a:https://agent.example", false, false),
        ("", false, true),
        ("acp", false, true),
        (" native", false, true),
    ];
    for provider in COORDINATES {
        for model in COORDINATES {
            for (backend, native, backend_invalid) in BACKENDS {
                let padded =
                    provider.starts_with(' ') || model.starts_with(' ') || backend.starts_with(' ');
                let half_native = native && (provider.is_empty() != model.is_empty());
                let expected_admit = !padded && !backend_invalid && !half_native;
                let parsed = ModelBinding::parse(provider, model, backend);
                assert_eq!(parsed.is_ok(), expected_admit);
                match parsed {
                    Ok(binding) => assert_eq!(binding.identity(), (provider, model, backend)),
                    Err(InvalidModelBinding::Whitespace { .. }) => assert!(padded),
                    Err(InvalidModelBinding::Backend(_)) => assert!(backend_invalid && !padded),
                    Err(InvalidModelBinding::IncompleteNative { .. }) => {
                        assert!(half_native && !padded && !backend_invalid)
                    }
                }
            }
        }
    }
}
