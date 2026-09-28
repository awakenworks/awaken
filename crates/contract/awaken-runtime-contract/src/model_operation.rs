//! Run-scoped evidence for actual model executor invocations, not usage or a
//! provider terminal receipt. The existing ThreadCommit log owns persistence.

use std::num::NonZeroU64;

use serde::{Deserialize, Serialize};

use crate::resolved::ModelBinding;
use crate::{MergePolicy, Scope};
use awaken_agent_contract::agent::state::StateKey;

/// The private witness carried by every minted proof. It is nameable only
/// inside this module, so no struct literal and no functional-update syntax
/// anywhere else in the workspace can assemble a [`TerminalReceipt`] or a
/// [`NoDispatchProof`]. Positive evidence therefore has exactly the entry
/// points published below, and `Unknown` stays free to construct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Seal;

/// Which wire fact a [`TerminalReceipt`] rests on, recorded in the run journal
/// so an operator reading history can see *why* an invocation was ruled
/// terminal instead of having to trust that some adapter once decided so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
#[serde(rename_all = "snake_case")]
pub enum TerminalWireFact {
    /// The provider named a terminal status for this operation on the wire.
    Status,
    /// A complete non-streaming response body was received and decoded.
    CompleteBody,
    /// A complete HTTP error response body was received for this invocation.
    CompleteErrorBody,
    /// The streaming protocol's own end-of-response frame was received.
    StreamEndFrame,
    /// Restored from a journal written before receipts were recorded. Such a
    /// record still proves some adapter established termination at the time;
    /// the wire fact it rested on simply was not kept. This variant is
    /// reachable only by decoding history, never by minting.
    Unrecorded,
}

/// The decoded frame an adapter must name to mint a receipt without a status
/// token. Only the adapter that owns a provider's codec can witness one, and
/// naming the exact frame at the call site is what a reviewer checks.
///
/// None of the following is an envelope, and every one of them must report
/// [`ModelExecutionEvidence::Unknown`]: a local timeout firing, a dropped or
/// abandoned future, a socket EOF arriving before the protocol's end frame, an
/// HTTP status line, a request id, or a transport error being returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseEnvelope {
    /// The provider's complete non-streaming response body was received and
    /// decoded for this exact invocation.
    CompleteBody,
    /// The provider completed this invocation with an HTTP error response and
    /// the adapter consumed its complete body. A status line alone is not this.
    CompleteErrorBody,
    /// The streaming protocol's own end-of-response frame was received
    /// (OpenAI `[DONE]`, Anthropic `message_stop`) — not a socket EOF.
    StreamEndFrame,
}

/// Why a claimed termination is not a receipt. Every variant means the caller
/// must report [`ModelExecutionEvidence::Unknown`]: re-entry stays blocked.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UnprovenTermination {
    /// The provider named no terminal status at all. Absence is never proof.
    #[error("the provider named no terminal status for this operation")]
    AbsentStatus,
    /// The provider named a status this platform does not accept as terminal.
    /// Guessing it into a terminal class is the exact fault this type exists
    /// to prevent, so it fails closed instead.
    #[error("`{0}` is not a status that proves the remote operation ended")]
    UnrecognizedStatus(String),
}

/// The complete set of provider status tokens this platform accepts as proof
/// that a remote operation ended. Supporting another dialect means adding its
/// tokens here, under review, in one place — not classifying strings inside an
/// adapter where a catch-all arm can quietly promote an unknown value.
const TERMINAL_WIRE_STATUS: &[&str] = &["completed", "incomplete", "failed", "cancelled"];

/// Positive proof that a remote operation ended, mintable only by parsing a
/// remote terminal receipt.
///
/// The private [`Seal`] makes this unconstructible outside this module: there
/// is no public field, no struct literal, no `Default`, and no zero-argument
/// constructor. The only ways to obtain one are
/// [`from_wire_status`](Self::from_wire_status), which requires a terminal
/// status token the provider itself put on the wire, and
/// [`from_response_envelope`](Self::from_response_envelope), which requires the
/// caller to name the decoded frame it is holding. A local timeout, a dropped
/// future, an HTTP status or a transport error has neither, so none of them can
/// produce terminal evidence any more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalReceipt {
    fact: TerminalWireFact,
    seal: Seal,
}

impl TerminalReceipt {
    /// Mint from the provider's own terminal status token, taken verbatim from
    /// a decoded response. An absent token, or one outside
    /// [`TERMINAL_WIRE_STATUS`], is an error and never a receipt.
    ///
    /// # Errors
    /// [`UnprovenTermination`] whenever the status cannot prove termination.
    pub fn from_wire_status(status: Option<&str>) -> Result<Self, UnprovenTermination> {
        let status = status.ok_or(UnprovenTermination::AbsentStatus)?;
        if !TERMINAL_WIRE_STATUS.contains(&status) {
            return Err(UnprovenTermination::UnrecognizedStatus(status.to_owned()));
        }
        Ok(Self {
            fact: TerminalWireFact::Status,
            seal: Seal,
        })
    }

    /// Mint from a decoded response envelope, for the providers whose terminal
    /// fact is the frame itself rather than a status token. The caller must
    /// hold that frame; see [`ResponseEnvelope`] for what does not qualify.
    #[must_use]
    pub const fn from_response_envelope(envelope: ResponseEnvelope) -> Self {
        Self {
            fact: match envelope {
                ResponseEnvelope::CompleteBody => TerminalWireFact::CompleteBody,
                ResponseEnvelope::CompleteErrorBody => TerminalWireFact::CompleteErrorBody,
                ResponseEnvelope::StreamEndFrame => TerminalWireFact::StreamEndFrame,
            },
            seal: Seal,
        }
    }

    /// The wire fact this receipt rests on, for diagnostics and history.
    #[must_use]
    pub const fn wire_fact(self) -> TerminalWireFact {
        self.fact
    }

    /// Restore evidence a trusted adapter minted before the wire fact was
    /// recorded. Private: reachable only from this module's decoder.
    const fn unrecorded() -> Self {
        Self {
            fact: TerminalWireFact::Unrecorded,
            seal: Seal,
        }
    }
}

/// Where in its own send path an adapter stopped, for a [`NoDispatchProof`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
#[serde(rename_all = "snake_case")]
pub enum PreDispatchStage {
    /// The neutral request could not be projected into the provider dialect,
    /// or its per-call options could not be assembled. Nothing was encoded.
    RequestProjection,
    /// No provider executor, endpoint or egress lease could be resolved for
    /// the binding, so no adapter was ever entered.
    ExecutorResolution,
    /// Runtime refused the live attempt before constructing or calling the
    /// provider executor, for example when cancellation arrived during the
    /// uncancellable entry commit. A resolved executor is not a dispatched call.
    RuntimeAdmission,
    /// The transport could not establish a connection for this request (name
    /// resolution, TCP connect or TLS handshake failed), so no byte of the
    /// request was written to any socket.
    TransportConnect,
    /// Restored from a journal written before proofs were recorded. Reachable
    /// only by decoding history, never by minting.
    Unrecorded,
}

/// Positive proof that a request never left this process.
///
/// Unlike termination there is no remote artifact to parse — the absence of a
/// dispatch has no wire form — so the adapter or Runtime names its exact
/// pre-call refusal. Each mintable stage is strictly before the first request
/// byte is written to a socket, which is the one
/// claim a caller downstream of a send can never honestly make. The private
/// [`Seal`] keeps this out of reach of struct literals just like a receipt;
/// dedicated constructors cannot mint the decoder-only `Unrecorded` tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoDispatchProof {
    stage: PreDispatchStage,
    seal: Seal,
}

impl NoDispatchProof {
    const fn mint(stage: PreDispatchStage) -> Self {
        Self { stage, seal: Seal }
    }

    /// The request could not be projected before transport admission.
    #[must_use]
    pub const fn request_projection() -> Self {
        Self::mint(PreDispatchStage::RequestProjection)
    }

    /// No executable provider/endpoint was resolved before transport admission.
    #[must_use]
    pub const fn executor_resolution() -> Self {
        Self::mint(PreDispatchStage::ExecutorResolution)
    }

    /// Runtime rejected the attempt before constructing a provider future.
    #[must_use]
    pub const fn runtime_admission() -> Self {
        Self::mint(PreDispatchStage::RuntimeAdmission)
    }

    /// The transport failed while establishing its connection, before writing
    /// any request byte. Minted only by the provider's wire classifier from
    /// the transport's own connect-phase error.
    #[must_use]
    pub const fn transport_connect() -> Self {
        Self::mint(PreDispatchStage::TransportConnect)
    }

    /// Which pre-send stage refused the request, for diagnostics and history.
    #[must_use]
    pub const fn stage(self) -> PreDispatchStage {
        self.stage
    }

    const fn unrecorded() -> Self {
        Self {
            stage: PreDispatchStage::Unrecorded,
            seal: Seal,
        }
    }
}

/// Evidence about one invocation, independent of response quality and error
/// classification. Missing evidence is unknown, including legacy Returned.
///
/// `Unknown` is deliberately free: "I do not know" is exactly what a timeout, a
/// dropped future or a transport error is entitled to report, and it is the
/// `Default`. The two positive variants instead carry a sealed proof that only
/// a trusted adapter can mint, so a local timeout, a dropped stream, an HTTP
/// status or a request id can no longer be written as termination at all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ModelExecutionEvidence {
    #[default]
    Unknown,
    NotDispatched(NoDispatchProof),
    Terminated(TerminalReceipt),
}

impl ModelExecutionEvidence {
    /// Physical permission only. Retry policy, current claim and budget remain
    /// independent mandatory gates owned by Runtime/Dispatch.
    #[must_use]
    pub const fn permits_reentry(self) -> bool {
        matches!(self, Self::NotDispatched(_) | Self::Terminated(_))
    }
}

const EVIDENCE_VARIANTS: &[&str] = &["Unknown", "NotDispatched", "Terminated"];

impl Serialize for ModelExecutionEvidence {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Unknown => {
                serializer.serialize_unit_variant("ModelExecutionEvidence", 0, EVIDENCE_VARIANTS[0])
            }
            Self::NotDispatched(proof) => serializer.serialize_newtype_variant(
                "ModelExecutionEvidence",
                1,
                EVIDENCE_VARIANTS[1],
                &proof.stage,
            ),
            Self::Terminated(receipt) => serializer.serialize_newtype_variant(
                "ModelExecutionEvidence",
                2,
                EVIDENCE_VARIANTS[2],
                &receipt.fact,
            ),
        }
    }
}

impl<'de> Deserialize<'de> for ModelExecutionEvidence {
    /// Decoding restores evidence a trusted adapter already minted and this run
    /// already committed; it is not a second minting authority, and it is the
    /// same trust boundary the committed journal has always been. A pre-receipt
    /// record decodes to the non-committal `Unrecorded` wire fact rather than
    /// having one invented for it, and an unknown tag fails closed.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(EvidenceVisitor)
    }
}

struct EvidenceVisitor;

impl<'de> serde::de::Visitor<'de> for EvidenceVisitor {
    type Value = ModelExecutionEvidence;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("model execution evidence")
    }

    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
        match value {
            "Unknown" => Ok(ModelExecutionEvidence::Unknown),
            "NotDispatched" => Ok(ModelExecutionEvidence::NotDispatched(
                NoDispatchProof::unrecorded(),
            )),
            "Terminated" => Ok(ModelExecutionEvidence::Terminated(
                TerminalReceipt::unrecorded(),
            )),
            other => Err(E::unknown_variant(other, EVIDENCE_VARIANTS)),
        }
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        use serde::de::Error as _;
        let Some(tag) = map.next_key::<String>()? else {
            return Err(A::Error::custom("model execution evidence has no variant"));
        };
        let value = match tag.as_str() {
            "Unknown" => {
                map.next_value::<serde::de::IgnoredAny>()?;
                ModelExecutionEvidence::Unknown
            }
            "NotDispatched" => ModelExecutionEvidence::NotDispatched(NoDispatchProof {
                stage: map.next_value()?,
                seal: Seal,
            }),
            "Terminated" => ModelExecutionEvidence::Terminated(TerminalReceipt {
                fact: map.next_value()?,
                seal: Seal,
            }),
            other => return Err(A::Error::unknown_variant(other, EVIDENCE_VARIANTS)),
        };
        if map.next_key::<serde::de::IgnoredAny>()?.is_some() {
            return Err(A::Error::custom(
                "model execution evidence carries exactly one variant",
            ));
        }
        Ok(value)
    }
}

/// Local observation only. In particular, a returned transport error or a
/// dropped timeout future does not prove remote computation has stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelOperationPhase {
    /// Entry intent committed before calling the executor. A crash between
    /// this commit and the call is deliberately indistinguishable from entry.
    Entered,
    Returned,
    TimedOut,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelOperation {
    pub ordinal: NonZeroU64,
    pub binding: ModelBinding,
    pub phase: ModelOperationPhase,
    #[serde(default)]
    pub execution: ModelExecutionEvidence,
}

impl ModelOperation {
    /// A retained partial requires positive predecessor evidence. Absence is
    /// admissible only for fresh inference, never as a historical stop proof.
    #[must_use]
    pub fn permits_reentry(previous: Option<&Self>, recovering_checkpoint: bool) -> bool {
        previous.map_or(!recovering_checkpoint, |operation| {
            operation.execution.permits_reentry()
        })
    }

    /// Sequence allocation never wraps and does not infer quiescence from any
    /// prior phase. Admission/physical exclusion still belongs to Dispatch.
    pub fn next(previous: Option<&Self>, binding: ModelBinding) -> Option<Self> {
        next_ordinal(previous.map_or(0, |value| value.ordinal.get())).map(|ordinal| Self {
            ordinal,
            binding,
            phase: ModelOperationPhase::Entered,
            execution: ModelExecutionEvidence::Unknown,
        })
    }
}

fn next_ordinal(previous: u64) -> Option<NonZeroU64> {
    previous.checked_add(1).and_then(NonZeroU64::new)
}

/// Identity derived from the existing Run-scoped model journal. This is trusted
/// execution context, never model input, a provider response id or a stop proof.
/// There is no second sequence allocator or independently persisted identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelOperationContext {
    thread_id: awaken_agent_contract::agent::thread::Id,
    run_id: awaken_agent_contract::agent::run::Id,
    ordinal: NonZeroU64,
    binding: ModelBinding,
}

impl ModelOperationContext {
    /// Runtime publishes this context only after the corresponding entry write
    /// succeeds. Recovery may reconstruct the same identity from that record;
    /// constructing it alone does not authorize dispatch or replay.
    #[must_use]
    pub fn from_operation(
        thread_id: &awaken_agent_contract::agent::thread::Id,
        run_id: &awaken_agent_contract::agent::run::Id,
        operation: &ModelOperation,
    ) -> Self {
        Self {
            thread_id: thread_id.clone(),
            run_id: run_id.clone(),
            ordinal: operation.ordinal,
            binding: operation.binding.clone(),
        }
    }

    #[must_use]
    pub fn run_id(&self) -> &awaken_agent_contract::agent::run::Id {
        &self.run_id
    }

    #[must_use]
    pub fn binding(&self) -> &ModelBinding {
        &self.binding
    }

    /// A versioned, bounded coordinate for infrastructure correlation. Hash the
    /// structured tuple so delimiters inside opaque ids cannot create aliases.
    /// Equality identifies one invocation, not permission to invoke it again.
    #[must_use]
    pub fn operation_id(&self) -> String {
        let fingerprint = crate::resolution::content_fingerprint(&(
            "model-operation-v1",
            &self.thread_id,
            &self.run_id,
            self.ordinal,
            &self.binding,
        ))
        .expect("model operation identity components always serialize");
        format!("model-op-v1:{fingerprint}")
    }
}

/// One invocation scope owns identity and its transient timing together. It
/// neither allocates another operation nor persists another source of truth.
struct ModelInvocationContext {
    operation: Option<ModelOperationContext>,
    timing: Option<crate::llm::ModelAttemptTiming>,
}

tokio::task_local! {
    static MODEL_OPERATION_CONTEXT: ModelInvocationContext;
}

/// Read the Runtime-owned identity at provider entry. Ephemeral calls must not
/// fabricate a durable identity merely because they have a timing budget.
#[must_use]
pub fn current_model_operation_context() -> Option<ModelOperationContext> {
    MODEL_OPERATION_CONTEXT
        .try_with(|context| context.operation.clone())
        .ok()
        .flatten()
}

#[must_use]
pub fn current_model_attempt_timing() -> Option<crate::llm::ModelAttemptTiming> {
    MODEL_OPERATION_CONTEXT
        .try_with(|context| context.timing)
        .ok()
        .flatten()
}

/// Explicit identity-only scopes mask both outer identity and outer timing.
/// This existing helper remains for directly instrumented adapter observations;
/// Runtime uses the complete invocation scope below for actual inference.
pub async fn with_model_operation_context<T>(
    context: Option<ModelOperationContext>,
    future: impl std::future::Future<Output = T>,
) -> T {
    MODEL_OPERATION_CONTEXT
        .scope(
            ModelInvocationContext {
                operation: context,
                timing: None,
            },
            future,
        )
        .await
}

/// Pass the same validated outer timing through entry/credential materialization
/// to the selected provider, without resolving that provider or its secrets twice.
pub async fn with_model_invocation_context<T>(
    operation: Option<ModelOperationContext>,
    timing: crate::llm::ModelAttemptTiming,
    future: impl std::future::Future<Output = T>,
) -> T {
    MODEL_OPERATION_CONTEXT
        .scope(
            ModelInvocationContext {
                operation,
                timing: Some(timing),
            },
            future,
        )
        .await
}

/// One latest-value projection; older invocations remain in the append-only
/// Thread state log. ToolBatch and model usage keep their existing owners.
pub struct ActiveModelOperation;

impl StateKey for ActiveModelOperation {
    const KEY: &'static str = "runtime.model_operation.v1";
    const SCOPE: Scope = Scope::Run;
    const MERGE: MergePolicy = MergePolicy::Disjoint;
    type Value = Option<ModelOperation>;
}

impl ActiveModelOperation {
    /// Reconstruct recorded invocations from an already frozen committed view.
    /// The caller owns prefix selection; this does not invent commit cursors or
    /// certify complete enrollment. Both Native recovery and historical protocol
    /// observation use this one decoder/fold instead of the latest-value cell.
    pub fn recorded<'a>(
        state: impl IntoIterator<Item = &'a awaken_agent_contract::agent::state::Command>,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<Vec<ModelOperation>, awaken_agent_contract::agent::state::StateError> {
        use awaken_agent_contract::agent::state::Store;
        let mut recorded = Vec::new();
        for command in state {
            if !Self::records_run(command, run_id) {
                continue;
            }
            if command.merge != Self::MERGE {
                return Err(invalid_history(
                    "model evidence has an incompatible merge policy",
                ));
            }
            // Decode EACH write through the existing typed cell. Folding first
            // would hide malformed/removed history behind a later valid Set.
            let mut store = Store::new();
            store.apply(command);
            let operation = Self::load(&store)?.ok_or_else(|| {
                invalid_history("model evidence cannot erase recorded invocations")
            })?;
            if !fold_recorded_operation(&mut recorded, operation) {
                return Err(invalid_history(
                    "model invocation ordinal changed its binding",
                ));
            }
        }
        Ok(recorded)
    }

    /// Read every recorded invocation of the exact Run at one committed prefix.
    /// Later ordinals cannot erase an earlier Unknown; updates to one ordinal
    /// must preserve its binding. Results are ordered by ordinal and reconstruct
    /// the original ModelOperationContext without allocating new identities.
    /// This is a transient view of ThreadCommit, not a receipt store. An empty
    /// result is NOT proof of no execution or complete participant enrollment.
    /// Later receipts cannot rewrite an already published historical diagnosis.
    pub fn recorded_at_commit(
        state: &[awaken_agent_contract::agent::state::Command],
        cursors: &[u64],
        run_id: &awaken_agent_contract::agent::run::Id,
        through: u64,
    ) -> Result<Vec<ModelOperation>, awaken_agent_contract::agent::state::StateError> {
        if !state
            .iter()
            .any(|command| Self::records_run(command, run_id))
        {
            return Ok(Vec::new());
        }
        if state.len() != cursors.len() {
            return Err(invalid_history(
                "model evidence has no aligned commit coordinates",
            ));
        }
        let mut previous_cursor = 0;
        for cursor in cursors {
            if *cursor == 0 || *cursor < previous_cursor {
                return Err(invalid_history(
                    "model evidence commit coordinates are not ordered",
                ));
            }
            previous_cursor = *cursor;
        }
        Self::recorded(
            state
                .iter()
                .zip(cursors)
                .filter_map(|(command, cursor)| (*cursor <= through).then_some(command)),
            run_id,
        )
    }

    fn records_run(
        command: &awaken_agent_contract::agent::state::Command,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> bool {
        command.scope == Self::SCOPE
            && command.key.0 == Self::KEY
            && command.run_id.as_ref() == Some(run_id)
    }
}

fn invalid_history(detail: &str) -> awaken_agent_contract::agent::state::StateError {
    awaken_agent_contract::agent::state::StateError {
        key: ActiveModelOperation::KEY.into(),
        scope: ActiveModelOperation::SCOPE,
        detail: detail.into(),
    }
}

/// One history fold, shared by the committed observer and its implementation
/// proof. Binary lookup keeps repeated receipts from scanning every prior call.
/// Rebinding rejects without changing either invocation or its neighbors.
fn fold_recorded_operation(recorded: &mut Vec<ModelOperation>, operation: ModelOperation) -> bool {
    match recorded.binary_search_by_key(&operation.ordinal, |record| record.ordinal) {
        Ok(index) => {
            if recorded[index].binding != operation.binding {
                return false;
            }
            recorded[index] = operation;
        }
        Err(index) => recorded.insert(index, operation),
    }
    true
}

/// Every positive-evidence value the proofs must exhaust, including each
/// recorded wire fact and pre-send stage. Built from this module's private
/// seal so the proof covers values a decoder can restore, not only the two
/// minting doors — the doors' own fail-closed rules are proven separately.
#[cfg(kani)]
fn any_evidence(selector: u8) -> ModelExecutionEvidence {
    match selector % 3 {
        0 => ModelExecutionEvidence::Unknown,
        1 => ModelExecutionEvidence::NotDispatched(NoDispatchProof {
            stage: kani::any(),
            seal: Seal,
        }),
        _ => ModelExecutionEvidence::Terminated(TerminalReceipt {
            fact: kani::any(),
            seal: Seal,
        }),
    }
}

/// The one door through which this module's fixtures build a model binding.
///
/// Test variants are admitted by the production parser, including every
/// independent identity-axis change. Kani's separately checked representative
/// avoids heap-string admission only in proofs; no production constructor forks.
#[cfg(test)]
fn fixture_binding(
    provider_identity_ref: impl Into<String>,
    model_ref: impl Into<String>,
    backend_ref: impl Into<String>,
) -> ModelBinding {
    ModelBinding::parse(provider_identity_ref, model_ref, backend_ref)
        .expect("fixture coordinates must satisfy the production admission")
}

#[cfg(kani)]
#[kani::proof]
fn no_dispatch_mints_only_reviewed_pre_call_stages() {
    // C1 the production caller chooses one of four explicit constructors;
    // E1 the sealed proof carries that exact pre-call stage; E2 no constructor
    // can mint decoder-only Unrecorded. Exhaust every u8 selector; this is a
    // constructor/stage proof, not transport honesty or async journal proof.
    let choice: u8 = kani::any();
    let proof = match choice % 4 {
        0 => NoDispatchProof::request_projection(),
        1 => NoDispatchProof::executor_resolution(),
        2 => NoDispatchProof::runtime_admission(),
        _ => NoDispatchProof::transport_connect(),
    };
    assert!(matches!(
        proof.stage(),
        PreDispatchStage::RequestProjection
            | PreDispatchStage::ExecutorResolution
            | PreDispatchStage::RuntimeAdmission
            | PreDispatchStage::TransportConnect
    ));
    assert_eq!(
        proof.stage(),
        match choice % 4 {
            0 => PreDispatchStage::RequestProjection,
            1 => PreDispatchStage::ExecutorResolution,
            2 => PreDispatchStage::RuntimeAdmission,
            _ => PreDispatchStage::TransportConnect,
        }
    );
    assert_ne!(proof.stage(), PreDispatchStage::Unrecorded);
    assert!(ModelExecutionEvidence::NotDispatched(proof).permits_reentry());
}

#[cfg(kani)]
#[kani::proof]
fn model_execution_reentry_requires_positive_evidence() {
    // R1 unknown => deny; R2 not dispatched / terminal => only the physical
    // gate opens. Retry policy and claim/budget remain separate authorities.
    let selector: u8 = kani::any();
    let evidence = any_evidence(selector);
    assert_eq!(evidence.permits_reentry(), selector % 3 != 0);
    // R3 missing journal + retained checkpoint => deny; R4 neither => fresh
    // entry allowed. Cross R1..R4 with each local phase: phase cannot authorize.
    // This exhausts the production decision, not persistence or adapter truth.
    let present: bool = kani::any();
    let recovering_checkpoint: bool = kani::any();
    let phase_selector: u8 = kani::any();
    let operation = ModelOperation {
        ordinal: NonZeroU64::MIN,
        binding: ModelBinding::proof_representative(false),
        phase: match phase_selector % 3 {
            0 => ModelOperationPhase::Entered,
            1 => ModelOperationPhase::Returned,
            _ => ModelOperationPhase::TimedOut,
        },
        execution: evidence,
    };
    assert_eq!(
        ModelOperation::permits_reentry(present.then_some(&operation), recovering_checkpoint),
        if present {
            selector % 3 != 0
        } else {
            !recovering_checkpoint
        }
    );
}

#[cfg(kani)]
#[kani::proof]
fn model_operation_ordinal_never_wraps_or_reuses_a_coordinate() {
    // C1 previous < MAX / C2 previous == MAX; E1 exact nonzero successor /
    // E2 refusal. R1=C1=>E1; R2=C2=>E2. Exhaust the production u64 kernel.
    let previous: u64 = kani::any();
    match next_ordinal(previous) {
        Some(next) => {
            assert!(previous < u64::MAX);
            assert!(next.get() > previous);
            assert_eq!(next.get() - previous, 1);
            // C3 adapter identity is projected from this record, not allocated
            // by a second counter. Exhaust the full-width ordinal projection;
            // string tuple isolation and task-local propagation use Rust tests.
            // This does not prove the fingerprint or any provider-side effect.
            let operation = ModelOperation {
                ordinal: next,
                binding: ModelBinding::proof_representative(false),
                phase: ModelOperationPhase::Entered,
                execution: ModelExecutionEvidence::Unknown,
            };
            let identity = ModelOperationContext::from_operation(
                &awaken_agent_contract::agent::thread::Id(String::new()),
                &awaken_agent_contract::agent::run::Id(String::new()),
                &operation,
            );
            assert_eq!(identity.ordinal, next);
        }
        None => assert_eq!(previous, u64::MAX),
    }
}

#[cfg(kani)]
#[kani::proof]
// The bound covers the two-element history fold and the byte comparison of the
// representative's longest coordinate ("native"): the binding is now a real
// admissible value, so equality runs the production `memcmp` instead of the
// zero-length one the removed `#[cfg(kani)]` fork used to produce.
#[kani::unwind(8)]
fn model_operation_history_preserves_distinct_subjects() {
    // Production fold, two arbitrary nonzero u64 ordinals, equal/different
    // bindings, and all evidence values. R1 new ordinal => retain both subjects;
    // R2 same ordinal+binding => replace only that observation; R3 same ordinal
    // with rebinding => reject with no mutation. Strings are fixed empty/one-byte
    // literals; decoding, arbitrary history length and provider truth are outside
    // this bounded implementation proof and remain executable/TCB boundaries.
    let first: u64 = kani::any();
    let second: u64 = kani::any();
    kani::assume(first > 0 && second > 0);
    let same_binding: bool = kani::any();
    let evidence = any_evidence(kani::any());
    let original = ModelOperation {
        ordinal: NonZeroU64::new(first).unwrap(),
        binding: ModelBinding::proof_representative(false),
        phase: ModelOperationPhase::Entered,
        execution: ModelExecutionEvidence::Unknown,
    };
    let incoming = ModelOperation {
        ordinal: NonZeroU64::new(second).unwrap(),
        binding: ModelBinding::proof_representative(!same_binding),
        phase: ModelOperationPhase::Returned,
        execution: evidence,
    };
    let mut recorded = vec![original.clone()];
    let accepted = fold_recorded_operation(&mut recorded, incoming.clone());
    if first == second {
        assert_eq!(accepted, same_binding);
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0], if same_binding { incoming } else { original });
    } else {
        assert!(accepted);
        assert_eq!(recorded.len(), 2);
        let first_index = usize::from(first > second);
        assert_eq!(recorded[first_index], original);
        assert_eq!(recorded[1 - first_index], incoming);
        assert!(recorded[0].ordinal < recorded[1].ordinal);
    }
    // R4 Native restoration uses EVERY recorded subject, not the last write:
    // only a same-subject accepted receipt can clear the original Unknown.
    // R5 a late lower-ordinal receipt cannot rewind the next invocation key.
    assert_eq!(
        recorded
            .iter()
            .all(|operation| operation.execution.permits_reentry()),
        first == second && same_binding && evidence.permits_reentry(),
    );
    assert_eq!(recorded.last().unwrap().ordinal.get(), first.max(second));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Simulate a provider receipt through the one public door an adapter
    /// uses. There is deliberately no test-only backdoor: a fixture that
    /// wants terminal evidence must produce the same wire fact production
    /// code would have to produce.
    fn terminated() -> ModelExecutionEvidence {
        ModelExecutionEvidence::Terminated(
            TerminalReceipt::from_wire_status(Some("completed")).unwrap(),
        )
    }

    fn not_dispatched() -> ModelExecutionEvidence {
        ModelExecutionEvidence::NotDispatched(NoDispatchProof::request_projection())
    }

    #[tokio::test]
    async fn model_identity_is_reconstructible_exact_and_task_scoped() {
        use awaken_agent_contract::agent::{run::Id as RunId, thread::Id as ThreadId};
        // I1 same persisted tuple reconstructed, regardless of local phase =>
        // same id. I2 change each coordinate independently => distinct id. I3
        // concurrent/nested scopes => isolated; I4 absent/direct scope => None.
        // Hash collision resistance is TCB; this checks serialization boundaries,
        // not provider idempotency, replay authorization or stop evidence.
        let thread = ThreadId("thread:a".into());
        let run = RunId("run:b".into());
        let record = ModelOperation::next(None, fixture_binding("p", "m", "b")).unwrap();
        let context = ModelOperationContext::from_operation(&thread, &run, &record);
        let mut restored: ModelOperation =
            serde_json::from_value(serde_json::to_value(&record).unwrap()).unwrap();
        restored.phase = ModelOperationPhase::Returned;
        restored.execution = terminated();
        assert_eq!(
            context,
            ModelOperationContext::from_operation(&thread, &run, &restored)
        );
        for field in 0..6 {
            let mut changed = context.clone();
            let (provider, model, backend) = changed.binding.identity();
            match field {
                0 => changed.thread_id.0.push('x'),
                1 => changed.run_id.0.push('x'),
                2 => changed.ordinal = NonZeroU64::new(2).unwrap(),
                3 => changed.binding = fixture_binding(format!("{provider}x"), model, backend),
                4 => changed.binding = fixture_binding(provider, format!("{model}x"), backend),
                _ => changed.binding = fixture_binding(provider, model, format!("{backend}x")),
            }
            assert_ne!(
                context.operation_id(),
                changed.operation_id(),
                "I2 field {field}"
            );
            let observe = |value: ModelOperationContext| async move {
                with_model_operation_context(Some(value.clone()), async {
                    tokio::task::yield_now().await;
                    assert_eq!(current_model_operation_context(), Some(value.clone()));
                    with_model_operation_context(None, async {
                        tokio::task::yield_now().await;
                        assert_eq!(current_model_operation_context(), None);
                    })
                    .await;
                    assert_eq!(current_model_operation_context(), Some(value));
                })
                .await;
            };
            tokio::join!(observe(context.clone()), observe(changed));
            assert_eq!(current_model_operation_context(), None);
        }
        let a = ModelOperationContext::from_operation(
            &ThreadId("a:b".into()),
            &RunId("c".into()),
            &record,
        );
        let b = ModelOperationContext::from_operation(
            &ThreadId("a".into()),
            &RunId("b:c".into()),
            &record,
        );
        assert_ne!(
            a.operation_id(),
            b.operation_id(),
            "I2 delimiters cannot alias"
        );
    }

    #[test]
    fn committed_model_observation_is_exact_run_and_prefix_scoped() {
        // C1 Run A/B, C2 boundary before/at/after write, C3 later receipt,
        // C4 malformed value, C5 unaligned cursors. R1 A at entry=>Unknown;
        // R2 A at receipt=>Terminated; R3 B/earlier=>absent; R4 C4/C5=>error.
        // Effects: later evidence cannot mutate the historical observation;
        // neither another Run nor corrupt persistence fabricates completion.
        use awaken_agent_contract::agent::run::Id;
        let a = Id("a".into());
        let b = Id("b".into());
        let mut operation = ModelOperation::next(None, fixture_binding("p", "m", "b")).unwrap();
        let mut entered = ActiveModelOperation::write(&Some(operation.clone()));
        entered.bind_run(&a);
        operation.execution = terminated();
        let mut returned = ActiveModelOperation::write(&Some(operation));
        returned.bind_run(&a);
        let commands = [entered, returned];
        for through in 0..=3 {
            for run in [&a, &b] {
                let mut value =
                    ActiveModelOperation::recorded_at_commit(&commands, &[1, 3], run, through)
                        .unwrap();
                assert!(value.len() <= 1);
                assert_eq!(
                    value.pop().map(|v| v.execution),
                    if run == &b || through == 0 {
                        None
                    } else if through < 3 {
                        Some(ModelExecutionEvidence::Unknown)
                    } else {
                        Some(terminated())
                    }
                );
            }
        }
        assert!(ActiveModelOperation::recorded_at_commit(&commands, &[1], &a, 2).is_err());
        let mut corrupt = commands[0].clone();
        corrupt.action =
            awaken_agent_contract::agent::state::Action::Set(serde_json::json!("corrupt"));
        assert!(ActiveModelOperation::recorded_at_commit(&[corrupt], &[1], &a, 2).is_err());
    }

    #[test]
    fn recorded_history_keeps_each_invocation_and_ignores_future_receipts() {
        // Causes: H1 distinct ordinals; H2 repeated exact receipt; H3 foreign,
        // unowned or wrong-scope command; H4 late receipt for an older ordinal;
        // H5 frozen commit cut. Effects: retain every H1 once; H2 idempotent;
        // H3 excluded; H4 changes only its subject at/after H5. No future receipt
        // can stop a historical participant. Empty is observed absence only,
        // never complete Native enrollment or permission to retire Dispatch.
        use awaken_agent_contract::agent::{run::Id, thread::Id as ThreadId};
        let run = Id("run".into());
        let thread = ThreadId("thread".into());
        let one = ModelOperation::next(None, fixture_binding("p", "one", "b")).unwrap();
        let mut two = ModelOperation::next(Some(&one), fixture_binding("p", "two", "b")).unwrap();
        two.execution = terminated();
        let mut three =
            ModelOperation::next(Some(&two), fixture_binding("p", "three", "b")).unwrap();
        three.execution = not_dispatched();
        let command = |operation: &ModelOperation| {
            let mut command = ActiveModelOperation::write(&Some(operation.clone()));
            command.bind_run(&run);
            command
        };
        let mut foreign = command(&one);
        foreign.bind_run(&Id("foreign".into()));
        let mut unowned = command(&one);
        unowned.run_id = None;
        let mut wrong_scope = command(&one);
        wrong_scope.scope = Scope::Thread;
        let mut returned_one = one.clone();
        returned_one.execution = terminated();
        let commands = [
            command(&one),
            foreign,
            unowned,
            wrong_scope,
            command(&two),
            command(&two),
            command(&three),
            command(&returned_one),
        ];
        for through in 0..=7 {
            let recorded = ActiveModelOperation::recorded_at_commit(
                &commands,
                &[1, 2, 2, 2, 3, 4, 5, 6],
                &run,
                through,
            )
            .unwrap();
            // R6 the current committed-view entry and historical prefix entry
            // must select exactly the same invocations. No synthesized cursors.
            let prefix = commands
                .iter()
                .zip([1, 2, 2, 2, 3, 4, 5, 6])
                .filter_map(|(command, cursor)| (cursor <= through).then_some(command));
            assert_eq!(
                ActiveModelOperation::recorded(prefix, &run).unwrap(),
                recorded
            );
            assert_eq!(
                recorded.len(),
                usize::from(through >= 1) + usize::from(through >= 3) + usize::from(through >= 5)
            );
            for (observed, original) in recorded.iter().zip([&one, &two, &three]) {
                assert_eq!(
                    ModelOperationContext::from_operation(&thread, &run, observed),
                    ModelOperationContext::from_operation(&thread, &run, original)
                );
            }
            if let Some(first) = recorded.first() {
                assert_eq!(
                    first.execution,
                    if through < 6 {
                        ModelExecutionEvidence::Unknown
                    } else {
                        terminated()
                    }
                );
            }
        }
    }

    #[test]
    fn damaged_history_cannot_be_hidden_by_a_later_valid_write() {
        // Causes: D1 binding drift in any identity component; D2 malformed/zero
        // ordinal/extra field; D3 Remove/null; D4 noncanonical merge; D5 cursor
        // zero, disorder or cardinality mismatch. D1-D5 => error, never a
        // partial successful inventory. A valid later Set cannot hide D1-D4;
        // cuts before that damaged write remain readable. Ordinary same-cursor
        // commands are positive controls in the multi-invocation test above.
        use awaken_agent_contract::agent::{run::Id, state::Action};
        let run = Id("run".into());
        let operation = ModelOperation::next(None, fixture_binding("p", "m", "b")).unwrap();
        let mut original = ActiveModelOperation::write(&Some(operation.clone()));
        original.bind_run(&run);
        for mode in 0..10 {
            let mut broken = original.clone();
            match mode {
                0..=2 => {
                    let mut other = operation.clone();
                    let (provider, model, backend) = other.binding.identity();
                    other.binding = match mode {
                        0 => fixture_binding(format!("{provider}x"), model, backend),
                        1 => fixture_binding(provider, format!("{model}x"), backend),
                        _ => fixture_binding(provider, model, format!("{backend}x")),
                    };
                    broken = ActiveModelOperation::write(&Some(other));
                    broken.bind_run(&run);
                }
                3 => broken.action = Action::Set(serde_json::json!("malformed")),
                4 | 5 => {
                    let Action::Set(value) = &mut broken.action else {
                        unreachable!()
                    };
                    if mode == 4 {
                        value["ordinal"] = 0.into();
                    } else {
                        value["unknown-field"] = true.into();
                    }
                }
                6 => broken.action = Action::Remove,
                7 => broken.action = Action::Set(serde_json::Value::Null),
                8 => broken.merge = MergePolicy::Commutative,
                _ => broken.merge = MergePolicy::Exclusive,
            }
            let commands = [original.clone(), broken, original.clone()];
            assert_eq!(
                ActiveModelOperation::recorded_at_commit(&commands, &[1, 2, 3], &run, 1).unwrap(),
                vec![operation.clone()]
            );
            assert!(
                ActiveModelOperation::recorded_at_commit(&commands, &[1, 2, 3], &run, 3).is_err(),
                "D mode {mode}"
            );
        }
        for cursors in [vec![], vec![1], vec![0, 1], vec![2, 1]] {
            assert!(
                ActiveModelOperation::recorded_at_commit(
                    &[original.clone(), original.clone()],
                    &cursors,
                    &run,
                    3
                )
                .is_err(),
                "D5 {cursors:?}"
            );
        }
    }

    #[test]
    fn recorded_history_fold_is_sorted_exact_and_rebinding_is_inert() {
        // F1 arbitrary insertion order and 1/MAX boundaries; F2 update an exact
        // subject; F3 rebind its ordinal. Effects: sorted unique inventory, F2
        // changes only its record, F3 rejects without mutating ANY record. The
        // larger 1024-record case exercises lookup beyond the proof's two-record
        // bound without claiming an asymptotic benchmark or provider truth.
        let make = |ordinal| ModelOperation {
            ordinal: NonZeroU64::new(ordinal).unwrap(),
            binding: fixture_binding("p", "m", "b"),
            phase: ModelOperationPhase::Entered,
            execution: ModelExecutionEvidence::Unknown,
        };
        let mut recorded = Vec::new();
        for ordinal in [u64::MAX, 1, u64::MAX - 1, 2]
            .into_iter()
            .chain((3..=1024).rev())
        {
            assert!(fold_recorded_operation(&mut recorded, make(ordinal)));
        }
        assert_eq!(recorded.len(), 1026);
        assert!(
            recorded
                .windows(2)
                .all(|pair| pair[0].ordinal < pair[1].ordinal)
        );
        for index in [0, 511, 1025] {
            let mut update = recorded[index].clone();
            update.execution = terminated();
            assert!(fold_recorded_operation(&mut recorded, update.clone()));
            let before = recorded.clone();
            assert_eq!(recorded[index], update);
            let (provider, model, backend) = update.binding.identity();
            update.binding = fixture_binding(provider, model, format!("{backend}x"));
            assert!(!fold_recorded_operation(&mut recorded, update));
            assert_eq!(recorded, before);
        }
    }

    #[test]
    fn terminal_evidence_is_mintable_only_from_a_parsed_remote_receipt() {
        // Causes: C1 the provider named a listed terminal status; C2 it named
        // an unlisted one; C3 it named none at all (a local timeout, a dropped
        // future, a transport error and an HTTP status line all land here,
        // because none of them carries a status token); C4 the adapter instead
        // decoded a response envelope it can name.
        // Effects: E1 a receipt recording the exact wire fact; E2 refusal.
        // Rules: R1 = C1 => E1(Status); R2 = C2 => E2; R3 = C3 => E2;
        // R4 = C4 => E1(CompleteBody/CompleteErrorBody/StreamEndFrame). Every refusal leaves the
        // caller with `Unknown`, which never permits re-entry — so an
        // unrecognised or absent status can no longer be guessed into a
        // terminal class the way a catch-all arm used to.
        for status in TERMINAL_WIRE_STATUS {
            let receipt = TerminalReceipt::from_wire_status(Some(status)).expect("R1");
            assert_eq!(receipt.wire_fact(), TerminalWireFact::Status);
            assert!(ModelExecutionEvidence::Terminated(receipt).permits_reentry());
        }
        for unlisted in [
            "in_progress",
            "queued",
            "COMPLETED",
            "completed ",
            "",
            "timeout",
        ] {
            assert_eq!(
                TerminalReceipt::from_wire_status(Some(unlisted)),
                Err(UnprovenTermination::UnrecognizedStatus(unlisted.to_owned())),
                "R2 {unlisted:?}"
            );
        }
        assert_eq!(
            TerminalReceipt::from_wire_status(None),
            Err(UnprovenTermination::AbsentStatus),
            "R3"
        );
        assert!(!ModelExecutionEvidence::Unknown.permits_reentry());
        for (envelope, fact) in [
            (
                ResponseEnvelope::CompleteBody,
                TerminalWireFact::CompleteBody,
            ),
            (
                ResponseEnvelope::CompleteErrorBody,
                TerminalWireFact::CompleteErrorBody,
            ),
            (
                ResponseEnvelope::StreamEndFrame,
                TerminalWireFact::StreamEndFrame,
            ),
        ] {
            let receipt = TerminalReceipt::from_response_envelope(envelope);
            assert_eq!(receipt.wire_fact(), fact, "R4");
            assert!(ModelExecutionEvidence::Terminated(receipt).permits_reentry());
        }
        // D1-D4 only reviewed pre-send constructors can mint positive proof;
        // D5 Unrecorded remains decoder-only. No general enum-taking public
        // constructor can turn D5 into new re-entry authority.
        for (stage, proof) in [
            (
                PreDispatchStage::RequestProjection,
                NoDispatchProof::request_projection(),
            ),
            (
                PreDispatchStage::ExecutorResolution,
                NoDispatchProof::executor_resolution(),
            ),
            (
                PreDispatchStage::RuntimeAdmission,
                NoDispatchProof::runtime_admission(),
            ),
            (
                PreDispatchStage::TransportConnect,
                NoDispatchProof::transport_connect(),
            ),
        ] {
            assert_eq!(proof.stage(), stage);
            assert!(ModelExecutionEvidence::NotDispatched(proof).permits_reentry());
        }
    }

    #[test]
    fn committed_evidence_round_trips_and_pre_receipt_history_stays_non_committal() {
        // Causes: W1 each minted evidence value; W2 a pre-receipt journal that
        // wrote the bare tag string; W3 a tagged record; W4 a corrupt tag,
        // a corrupt payload, or two variants in one record.
        // Effects: E1 exact round trip; E2 the recorded wire fact/stage is
        // readable; E3 W2 restores as Unrecorded — the record still proves an
        // adapter established the class, but no wire fact is invented for it;
        // E4 W4 fails closed rather than decoding to some default.
        let cases = [
            (
                ModelExecutionEvidence::Terminated(
                    TerminalReceipt::from_wire_status(Some("failed")).unwrap(),
                ),
                serde_json::json!({ "Terminated": "status" }),
            ),
            (
                ModelExecutionEvidence::Terminated(TerminalReceipt::from_response_envelope(
                    ResponseEnvelope::CompleteBody,
                )),
                serde_json::json!({ "Terminated": "complete_body" }),
            ),
            (
                ModelExecutionEvidence::Terminated(TerminalReceipt::from_response_envelope(
                    ResponseEnvelope::CompleteErrorBody,
                )),
                serde_json::json!({ "Terminated": "complete_error_body" }),
            ),
            (
                ModelExecutionEvidence::Terminated(TerminalReceipt::from_response_envelope(
                    ResponseEnvelope::StreamEndFrame,
                )),
                serde_json::json!({ "Terminated": "stream_end_frame" }),
            ),
            (
                ModelExecutionEvidence::NotDispatched(NoDispatchProof::executor_resolution()),
                serde_json::json!({ "NotDispatched": "executor_resolution" }),
            ),
            (
                ModelExecutionEvidence::NotDispatched(NoDispatchProof::transport_connect()),
                serde_json::json!({ "NotDispatched": "transport_connect" }),
            ),
            (
                ModelExecutionEvidence::Unknown,
                serde_json::json!("Unknown"),
            ),
        ];
        for (value, wire) in cases {
            assert_eq!(serde_json::to_value(value).unwrap(), wire, "W1/W3");
            assert_eq!(
                serde_json::from_value::<ModelExecutionEvidence>(wire).unwrap(),
                value,
                "E1"
            );
        }
        for (legacy, fact, stage) in [
            ("Terminated", Some(TerminalWireFact::Unrecorded), None),
            ("NotDispatched", None, Some(PreDispatchStage::Unrecorded)),
        ] {
            let decoded: ModelExecutionEvidence =
                serde_json::from_value(serde_json::json!(legacy)).unwrap();
            assert!(decoded.permits_reentry(), "W2 keeps the historical class");
            match decoded {
                ModelExecutionEvidence::Terminated(receipt) => {
                    assert_eq!(Some(receipt.wire_fact()), fact, "E3");
                }
                ModelExecutionEvidence::NotDispatched(proof) => {
                    assert_eq!(Some(proof.stage()), stage, "E3");
                }
                ModelExecutionEvidence::Unknown => panic!("W2 must not erase the class"),
            }
        }
        assert!(
            serde_json::from_value::<ModelExecutionEvidence>(serde_json::json!("Returned"))
                .unwrap_err()
                .to_string()
                .contains("unknown variant"),
            "E4 tag"
        );
        for corrupt in [
            serde_json::json!({ "Terminated": "handed_to_transport" }),
            serde_json::json!({ "NotDispatched": "sent" }),
            serde_json::json!({ "Terminated": "status", "Unknown": null }),
            serde_json::json!({ "Returned": "status" }),
            serde_json::json!(3),
        ] {
            assert!(
                serde_json::from_value::<ModelExecutionEvidence>(corrupt.clone()).is_err(),
                "E4 {corrupt}"
            );
        }
        // E5 a wholly foreign shape says what it wanted. Evidence corruption is
        // diagnosed from a run journal long after the fact, so the refusal has
        // to name the subject rather than leave an operator guessing which
        // cell rejected -- and guessing is how invented evidence gets written.
        assert!(
            serde_json::from_value::<ModelExecutionEvidence>(serde_json::json!(3))
                .unwrap_err()
                .to_string()
                .contains("expected model execution evidence"),
            "E5"
        );
    }

    #[test]
    fn legacy_local_phases_never_create_termination_evidence() {
        // L1 old phase is Entered/Returned/TimedOut; L2 evidence field absent;
        // L3 explicit evidence Unknown/NotDispatched/Terminated. Effects: all
        // L1+L2 decode Unknown; L3 round-trips exactly, independent of phase.
        // A local Returned is never promoted to remote termination on upgrade.
        for phase in [
            ModelOperationPhase::Entered,
            ModelOperationPhase::Returned,
            ModelOperationPhase::TimedOut,
        ] {
            let mut operation = ModelOperation::next(None, fixture_binding("p", "m", "b")).unwrap();
            operation.phase = phase;
            let mut legacy = serde_json::to_value(&operation).unwrap();
            legacy.as_object_mut().unwrap().remove("execution");
            let decoded: ModelOperation = serde_json::from_value(legacy).unwrap();
            assert_eq!(decoded.execution, ModelExecutionEvidence::Unknown);
            assert!(!decoded.execution.permits_reentry());
            for execution in [
                ModelExecutionEvidence::Unknown,
                not_dispatched(),
                terminated(),
            ] {
                operation.execution = execution;
                assert_eq!(
                    serde_json::from_value::<ModelOperation>(
                        serde_json::to_value(&operation).unwrap()
                    )
                    .unwrap(),
                    operation
                );
            }
        }
    }

    #[test]
    fn execution_evidence_does_not_change_failure_taxonomy_or_retry_hints() {
        use crate::llm::Error;
        use crate::resilience::Classify;
        // C1 every closed error class; C2 absent or each evidence value.
        // E1 absent => Unknown; E2 code, message, health, routing and delay
        // are preserved for all C1*C2; E3 only evidence controls physical
        // re-entry. Wrapping an already annotated error remains flat.
        for execution in [
            ModelExecutionEvidence::Unknown,
            not_dispatched(),
            terminated(),
        ] {
            for error in [
                Error::Binding("binding".into()),
                Error::Provider("provider".into()),
                Error::Timeout("timeout".into()),
                Error::ContextOverflow("context".into()),
                Error::InvalidRequest("request".into()),
                Error::Unauthorized("auth".into()),
                Error::LoginRequired("login".into()),
                Error::ModelNotFound("model".into()),
                Error::ContentFiltered("filter".into()),
                Error::RateLimited {
                    message: "rate".into(),
                    retry_after: Some(std::time::Duration::from_secs(2)),
                },
                Error::Overloaded {
                    message: "overload".into(),
                    retry_after: Some(std::time::Duration::from_secs(3)),
                },
                Error::UsageLimit {
                    message: "usage".into(),
                    reset_after: Some(std::time::Duration::from_secs(4)),
                },
            ] {
                assert_eq!(error.execution_evidence(), ModelExecutionEvidence::Unknown);
                let expected = (
                    error.code(),
                    error.to_string(),
                    error.is_retryable(),
                    error.retry_after(),
                    error.reset_hint(),
                    error.disposition(),
                );
                let observed = error
                    .with_execution_evidence(execution)
                    .with_execution_evidence(execution);
                assert_eq!(
                    (
                        observed.code(),
                        observed.to_string(),
                        observed.is_retryable(),
                        observed.retry_after(),
                        observed.reset_hint(),
                        observed.disposition()
                    ),
                    expected
                );
                assert_eq!(observed.execution_evidence(), execution);
                let Error::Observed { source, .. } = observed else {
                    panic!("explicit evidence required")
                };
                assert!(!matches!(*source, Error::Observed { .. }));
            }
        }
    }

    #[test]
    fn model_operation_coordinate_and_wire_decision_table() {
        // Causes: C1 absent/ordinary/MAX predecessor; C2 any local phase;
        // C3 zero ordinal on wire. Effects: E1 exact successor; E2 MAX rejects;
        // E3 malformed zero rejects. R1=!MAX+C2=>E1; R2=MAX+C2=>E2;
        // R3=C3=>E3. No phase, including Returned, is termination authority.
        let binding = fixture_binding("provider", "model", "adapter");
        for previous in [0, 1, u64::MAX - 1, u64::MAX] {
            for phase in [
                ModelOperationPhase::Entered,
                ModelOperationPhase::Returned,
                ModelOperationPhase::TimedOut,
            ] {
                let prior = NonZeroU64::new(previous).map(|ordinal| ModelOperation {
                    ordinal,
                    binding: binding.clone(),
                    phase,
                    execution: ModelExecutionEvidence::Unknown,
                });
                let next = ModelOperation::next(prior.as_ref(), binding.clone());
                assert_eq!(
                    next.as_ref().map(|v| v.ordinal.get()),
                    previous.checked_add(1)
                );
                if let Some(next) = next {
                    assert_eq!(next.phase, ModelOperationPhase::Entered);
                    let mut wire = serde_json::to_value(&next).unwrap();
                    assert_eq!(
                        serde_json::from_value::<ModelOperation>(wire.clone()).unwrap(),
                        next
                    );
                    wire["ordinal"] = 0.into();
                    assert!(serde_json::from_value::<ModelOperation>(wire).is_err());
                }
            }
        }
    }
}
