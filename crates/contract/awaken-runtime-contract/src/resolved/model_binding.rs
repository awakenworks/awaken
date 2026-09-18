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

/// The backend class the admission rule distinguishes; the exact reference is
/// a payload of the refusal, not an input to the decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendClass {
    /// The host's own executor: both account coordinates are required together.
    Native,
    /// An ACP or A2A executor that owns its account and may leave either blank.
    External,
    /// Not an executable backend reference at all.
    Invalid,
}

/// Everything the admission decision reads, and nothing it does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AdmissionFacts {
    /// Some coordinate carries surrounding whitespace.
    padded: bool,
    provider_empty: bool,
    model_empty: bool,
    class: BackendClass,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refusal {
    Whitespace,
    Backend,
    IncompleteNative,
}

/// The one admission rule both doors apply. Whitespace is judged first so a
/// padded reference is reported as padding rather than as an unknown backend;
/// then the backend must be executable; then a native route must state both
/// account coordinates or neither (a host-installed executor names itself).
const fn admission(facts: AdmissionFacts) -> Result<(), Refusal> {
    if facts.padded {
        return Err(Refusal::Whitespace);
    }
    match facts.class {
        BackendClass::Invalid => Err(Refusal::Backend),
        BackendClass::Native if facts.provider_empty != facts.model_empty => {
            Err(Refusal::IncompleteNative)
        }
        BackendClass::Native | BackendClass::External => Ok(()),
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
        let padded = [
            ("provider identity", &provider_identity_ref),
            ("model", &model_ref),
            ("backend", &backend_ref),
        ]
        .into_iter()
        .find(|(_, value)| value.trim() != value.as_str());
        let class = match Backend::from_ref(&backend_ref) {
            Backend::Invalid(_) => BackendClass::Invalid,
            Backend::Native => BackendClass::Native,
            Backend::Acp(_) | Backend::Remote(_) => BackendClass::External,
        };
        match admission(AdmissionFacts {
            padded: padded.is_some(),
            provider_empty: provider_identity_ref.is_empty(),
            model_empty: model_ref.is_empty(),
            class,
        }) {
            Ok(()) => {}
            Err(Refusal::Whitespace) => {
                let (coordinate, value) = padded.expect("a padded coordinate was found");
                return Err(InvalidModelBinding::Whitespace {
                    coordinate,
                    value: value.clone(),
                });
            }
            Err(Refusal::Backend) => return Err(InvalidModelBinding::Backend(backend_ref)),
            Err(Refusal::IncompleteNative) => {
                return Err(InvalidModelBinding::IncompleteNative {
                    provider_identity_ref,
                    model_ref,
                });
            }
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
        // Under Kani the string admission (byte loops over the coordinates,
        // heap-allocated `String`s) makes every proof that merely *holds* a
        // binding intractable, and none of those proofs claims admission:
        // the admission decision is proven separately as the pure kernel
        // `admission`, and the executable table/property tests run the real
        // string path. So proofs build the literal directly.
        #[cfg(kani)]
        {
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
        #[cfg(not(kani))]
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

    #[test]
    fn the_durable_ingress_refuses_every_under_specified_binding() {
        // The producer ratchet only holds the *number* of unvalidated
        // constructors down. What makes an under-specified binding
        // unrepresentable in a restored aggregate is this: the wire has no
        // door of its own, so a snapshot written before the completeness rule
        // -- or by any other writer -- fails closed on the way in rather than
        // reaching a comparison fence.
        //
        // C1 the stored object states complete coordinates; C2 its backend is
        // executable; C3 no coordinate is padded. E1 decode, E2 refuse.
        // R1 C1 & C2 & C3 => E1. R2 !C1 => E2. R3 !C2 => E2. R4 !C3 => E2.
        let decode = |provider: &str, model: &str, backend: &str| {
            serde_json::from_value::<ModelBinding>(serde_json::json!({
                "provider_identity_ref": provider,
                "model_ref": model,
                "backend_ref": backend,
            }))
        };
        assert_eq!(
            decode("anthropic", "claude-opus-4", "native").expect("R1"),
            ModelBinding::parse("anthropic", "claude-opus-4", "native").expect("R1"),
        );
        for (rule, provider, model, backend) in [
            ("R2 half a native binding", "anthropic", "", "native"),
            ("R2 the other half", "", "claude-opus-4", "native"),
            ("R3 inexecutable backend", "p", "m", ""),
            ("R4 padded backend", "p", "m", " native"),
            ("R4 padded provider", " p", "m", "native"),
        ] {
            let refusal = decode(provider, model, backend)
                .expect_err(&format!("{rule}: the durable ingress must refuse"));
            assert!(
                !refusal.to_string().is_empty(),
                "{rule}: the refusal names why"
            );
        }
    }
}

/// Kani exhausts the admission kernel over every fact combination: 2 (padded)
/// x 2 (provider empty) x 2 (model empty) x 3 (backend class) = 24 rows. The
/// two effects are E1 admit and E2 refuse with exactly one named reason.
///
/// | Rule | padded | provider | model | class    | Effect                |
/// | A1   | F      | set      | set   | native   | E1                    |
/// | A2   | F      | empty    | empty | native   | E1 (host-installed)   |
/// | A3   | F      | any      | any   | external | E1                    |
/// | A4   | F      | empty    | set   | native   | E2 IncompleteNative   |
/// | A5   | F      | set      | empty | native   | E2 IncompleteNative   |
/// | A6   | F      | any      | any   | invalid  | E2 Backend            |
/// | A7   | T      | any      | any   | any      | E2 Whitespace         |
///
/// `parse` derives the facts from the coordinate strings and attaches the
/// refusal payloads; that derivation is covered by the executable table and
/// property tests in this module, which run the real string code.
#[cfg(kani)]
#[kani::proof]
fn model_binding_admission_is_exactly_the_documented_rule_table() {
    let padded: bool = kani::any();
    let provider_empty: bool = kani::any();
    let model_empty: bool = kani::any();
    let class = match kani::any::<u8>() % 3 {
        0 => BackendClass::Native,
        1 => BackendClass::External,
        _ => BackendClass::Invalid,
    };
    let facts = AdmissionFacts {
        padded,
        provider_empty,
        model_empty,
        class,
    };
    let expected = if padded {
        Err(Refusal::Whitespace)
    } else if class == BackendClass::Invalid {
        Err(Refusal::Backend)
    } else if class == BackendClass::Native && provider_empty != model_empty {
        Err(Refusal::IncompleteNative)
    } else {
        Ok(())
    };
    assert_eq!(admission(facts), expected);
}
