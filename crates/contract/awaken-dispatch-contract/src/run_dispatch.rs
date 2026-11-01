// @formal entrypoint delegation.independent_child_scheduling_lease
// @formal entrypoint session_run.reservation_never_executes
// @formal entrypoint worker.durable_dispatch_contains_no_plaintext_secret
// @formal entrypoint worker.retry_preserves_pinned_binding
//! One Run accepted into durable dispatch.
//!
//! [`RunDispatch`] is exactly the data a durable queue persists and replays. It
//! carries no live handles (G3/G4), so any worker can rebuild the Run after a
//! crash. Live worker dependencies stay in the `awaken-dispatch` host.

use awaken_agent_contract::ExecutionScopeRef;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::credential::PlaintextHolder;
use awaken_provisioning_contract::SandboxCapacityShapeId;
use awaken_runtime_contract::activation::RunActivation;
use awaken_session_types::{SessionRunCommandFingerprint, SessionRunReplacement};
use awaken_worker_contract::PlacementRequirements;
use serde::{Deserialize, Serialize};

/// Dispatch-neutral envelope for a frozen Session resource manifest.
///
/// The dispatch bounded context owns delivery, not Session resource vocabulary,
/// so the already-resolved manifest crosses as canonical serialized data. The
/// Runtime Host decodes it through the Session contract before any sandbox is
/// opened. `workspace_id` remains explicit so claim-time scope equality can be
/// checked without interpreting the payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionResourceEnvelope {
    pub workspace_id: String,
    /// Opaque Session-owned resource generation used only for ordered replay.
    #[serde(default)]
    pub resource_revision: u64,
    pub resolved_resources_json: String,
}

impl SessionResourceEnvelope {
    #[must_use]
    pub fn new(
        workspace_id: impl Into<String>,
        resolved_resources_json: impl Into<String>,
    ) -> Self {
        Self {
            workspace_id: workspace_id.into(),
            resource_revision: 0,
            resolved_resources_json: resolved_resources_json.into(),
        }
    }

    #[must_use]
    pub fn at_revision(
        workspace_id: impl Into<String>,
        resource_revision: u64,
        resolved_resources_json: impl Into<String>,
    ) -> Self {
        Self {
            workspace_id: workspace_id.into(),
            resource_revision,
            resolved_resources_json: resolved_resources_json.into(),
        }
    }

    /// Restore the carried manifest through its owner's codec, which receives
    /// the Workspace, generation and resource string exactly as persisted.
    /// Dispatch never interprets the payload itself.
    pub fn decode_with<M, E>(
        &self,
        decode: impl FnOnce(&str, u64, &str) -> Result<M, E>,
    ) -> Result<M, E> {
        decode(
            &self.workspace_id,
            self.resource_revision,
            &self.resolved_resources_json,
        )
    }
}

/// Dispatch-neutral envelope for the immutable Session runtime projection.
///
/// Environment and tool-policy vocabulary remain owned by the Session bounded
/// context. Dispatch persists only canonical, secret-free serialized data so a
/// cold remote Worker can reconstruct the same eager/deferred provisioning
/// decision without consulting mutable Control state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRuntimeEnvelope {
    pub projection_json: String,
}

impl SessionRuntimeEnvelope {
    #[must_use]
    pub fn new(projection_json: impl Into<String>) -> Self {
        Self {
            projection_json: projection_json.into(),
        }
    }

    /// Restore the carried projection through its owner's codec. Dispatch
    /// never interprets the payload itself.
    pub fn decode_with<P, E>(&self, decode: impl FnOnce(&str) -> Result<P, E>) -> Result<P, E> {
        decode(&self.projection_json)
    }
}

/// Which admission command owns replay identity for one persisted dispatch.
/// The default preserves the historical full-dispatch rule. The dedicated
/// Session reservation port marks only its own rows as Session commands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchIdentityScope {
    #[default]
    FullDispatch,
    SessionCommand,
    /// A trusted, complete prepared activation. Unlike ordinary Session
    /// commands, it has no reduced pre-fingerprint replay relation.
    PreparedSessionCommand,
}

impl DispatchIdentityScope {
    fn is_full_dispatch(scope: &Self) -> bool {
        matches!(scope, Self::FullDispatch)
    }
}

/// The durable, serializable record of an accepted run. It holds no `Arc<dyn ...>`,
/// registry, or live handle (G3); the runtime builds live execution objects from
/// the activation's pinned snapshot on each attempt (G4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunDispatch {
    pub activation: RunActivation,
    /// Port-owned replay semantics persisted with the request. This prevents a
    /// self-affine continuation and a pre-activity Session reservation from
    /// being guessed apart later from the same routing shape.
    #[serde(
        default,
        skip_serializing_if = "DispatchIdentityScope::is_full_dispatch"
    )]
    pub identity_scope: DispatchIdentityScope,
    /// Immutable Session-application command identity computed before mutable
    /// Runtime projection. Older durable rows omit it; only a stored omission
    /// selects the legacy reservation comparison.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_command_fingerprint: Option<SessionRunCommandFingerprint>,
    /// Session whose runtime capabilities and commit/history boundary must drive
    /// this Run after recovery. Ordinary Runs omit it and route by their own
    /// thread. A child Run names its parent's session while retaining its own
    /// activation thread and first-class lifecycle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_thread_id: Option<ThreadId>,
    /// Queue-owned Session activity coordinate for the next coordinated child
    /// boundary. Ordinary and legacy Runs omit it. It is mutable across durable
    /// Awaiting replies and is therefore excluded from caller-owned Run identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_activity_epoch: Option<u64>,
    /// Immutable newest-wins intent authored by the Session application. It is
    /// persisted on the unclaimable reservation so crash recovery cannot turn
    /// a replacement into an ordinary append.
    #[serde(default)]
    pub session_run_replacement: SessionRunReplacement,
    /// W3C `traceparent` captured when the run was admitted, so a durably-dispatched
    /// execution continues the admitting request's distributed trace across the
    /// queue boundary. Absent when admitted without an active trace (or by an older
    /// writer): a pre-existing queue row simply deserializes it as `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traceparent: Option<String>,
    /// Authorized execution ownership resolved at the ingress edge. The dispatch
    /// aggregate treats it as an opaque coordinate and never derives it from a
    /// thread id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_scope: Option<ExecutionScopeRef>,
    /// Frozen Session resource input for cold-worker activation. The value is
    /// secret-free and carries only the intrinsic Workspace partition plus the
    /// already-resolved resource configuration. A worker installs it before opening
    /// the Session environment; it must never re-read current Agent bindings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_resources: Option<SessionResourceEnvelope>,
    /// Frozen Environment and Session-local tool-policy projection used by a
    /// cold Worker before context construction. Older rows omit it and retain
    /// their historical eager behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_runtime: Option<SessionRuntimeEnvelope>,
    /// Complete immutable publication closure required by this activation's
    /// non-root execution graph (delegates and extension-authored auxiliary
    /// Agents). The activation already carries the parent snapshot; this bundle
    /// contains only its non-self targets and crosses to a cold Worker instead
    /// of granting that Worker mutable catalog access.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agent_publications: Vec<awaken_runtime_contract::ExecutableAgentSnapshot>,
    /// Provider-neutral Environment creation shape. This is a placement
    /// preference only: workers without a ready receipt remain eligible and use
    /// the canonical cold path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_environment_shape: Option<SandboxCapacityShapeId>,
    /// Task observations this Run folds at StepStart, carried privately from
    /// the Task holder through the Session application. The claiming process
    /// offers them to its Task owner before execution; they grant no Task
    /// tenure. Ordinary Runs and older rows omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_task_input: Option<awaken_session_application_contract::PrivateTaskInput>,
    /// Exact Environment/deployment request for inference credential plaintext.
    /// Claim admission validates it against every selected published candidate
    /// and the selected Worker's installed capabilities before persisting the
    /// attempt-epoch bindings. `None` is valid only when no selected candidate
    /// carries a credential.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inference_plaintext_holder: Option<PlaintextHolder>,
    /// Hard worker requirements pinned at admission. Older durable rows omit this
    /// field and deserialize through the contract's explicit legacy posture;
    /// strict remote callers attach `PlacementRequirements::remote_required()`.
    #[serde(
        default,
        skip_serializing_if = "PlacementRequirements::is_legacy_default"
    )]
    pub placement: PlacementRequirements,
}

/// The BackgroundTask extension's plugin id as frozen into a snapshot's
/// `plugin_ids` (`awaken_ext_background_task::BACKGROUND_TASK_PLUGIN_ID`; the
/// Session executor's tests hold the two equal).
const BACKGROUND_TASK_PLUGIN_ID: &str = "background_task";

/// Closed classification of the Session coordinates carried by one dispatch.
///
/// This is descriptive evidence, not a second lifecycle. Admission ports use it
/// to reject a Session root intent at every executable enqueue path while the
/// dedicated reservation port accepts exactly that shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchAdmissionShape {
    OrdinaryRoot,
    SessionRootAwaitingActivity,
    SessionRootWithActivity,
    SessionChild,
    InvalidZeroActivityEpoch,
}

/// Classify the already-decoded routing coordinates without owning any Run or
/// Session lifecycle. `RunDispatch::admission_shape` is the sole caller in
/// production; keeping the categorical decision heap-free lets Kani exhaust it
/// without replacing the durable dispatch or its identity fields.
const fn classify_dispatch_admission(
    has_session_thread: bool,
    session_thread_matches: bool,
    session_activity_epoch: Option<u64>,
) -> DispatchAdmissionShape {
    if matches!(session_activity_epoch, Some(0)) {
        return DispatchAdmissionShape::InvalidZeroActivityEpoch;
    }
    if !has_session_thread {
        DispatchAdmissionShape::OrdinaryRoot
    } else if session_thread_matches {
        if session_activity_epoch.is_some() {
            DispatchAdmissionShape::SessionRootWithActivity
        } else {
            DispatchAdmissionShape::SessionRootAwaitingActivity
        }
    } else {
        DispatchAdmissionShape::SessionChild
    }
}

impl RunDispatch {
    /// Whether this durable request carries Session-owned execution state and
    /// therefore requires the subordinate Work fence on registered transport.
    #[must_use]
    pub fn requires_session_work_authority(&self) -> bool {
        self.session_thread_id.is_some()
            || self.session_resources.is_some()
            || self.session_runtime.is_some()
    }

    /// Classify the immutable routing/activity coordinates before persistence.
    /// The caller still owns port-specific authorization; this method only makes
    /// the previously implicit shape distinction total and shared.
    #[must_use]
    pub fn admission_shape(&self) -> DispatchAdmissionShape {
        classify_dispatch_admission(
            self.session_thread_id.is_some(),
            self.session_thread_id
                .as_ref()
                .is_some_and(|session| session == self.thread_id()),
            self.session_activity_epoch,
        )
    }

    fn canonicalized(&self) -> Self {
        let mut canonical = self.clone();
        canonical.traceparent = None;
        // A coordinated child rotates this queue-owned coordinate whenever an
        // Awaiting Run accepts another durable reply. Exact enqueue replays
        // never overwrite the stored request, so excluding it from identity
        // cannot grant a caller authority to change the current epoch.
        canonical.session_activity_epoch = None;
        canonical
    }

    fn canonical_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(&self.canonicalized())
            .expect("RunDispatch's serializable contract has no fallible value")
    }

    /// Stable identity of the Session-owned command that created a root Run
    /// reservation. The Session application owns the operation and input; the
    /// first accepted dispatch owns every resolved execution projection. A cold
    /// retry may therefore reconstruct different current Agent, model-routing,
    /// Resource, Runtime, Environment, or placement projections, but it must
    /// neither conflict with nor overwrite the frozen request already stored.
    fn legacy_session_reservation_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(&(
            &self.activation.run_id,
            &self.activation.thread_id,
            &self.activation.input,
            &self.activation.delegation_origin,
            &self.activation.data_subject_id,
            &self.activation.tool_capability_narrowing(),
            &self.session_thread_id,
        ))
        .expect("Session reservation identity has no fallible serializable value")
    }

    pub fn new(activation: RunActivation) -> Self {
        Self {
            activation,
            identity_scope: DispatchIdentityScope::FullDispatch,
            session_command_fingerprint: None,
            session_thread_id: None,
            session_activity_epoch: None,
            session_run_replacement: SessionRunReplacement::PreservePrior,
            traceparent: None,
            execution_scope: None,
            session_resources: None,
            session_runtime: None,
            agent_publications: Vec::new(),
            preferred_environment_shape: None,
            private_task_input: None,
            inference_plaintext_holder: None,
            placement: PlacementRequirements::default(),
        }
    }

    /// Stable dispatch identity for caller-owned Run ids. Distributed tracing is
    /// intentionally excluded: a retry may arrive under another request span,
    /// while every execution-bearing field must remain byte-equivalent.
    #[must_use]
    pub fn canonical_fingerprint(&self) -> String {
        let fingerprint = awaken_agent_contract::content_fingerprint(&self.canonicalized())
            .expect("RunDispatch's serializable contract has no fallible value");
        format!("sha256:{fingerprint}")
    }

    /// Canonical-byte replay comparison of an incoming retry (`other`) with
    /// this stored row. Hashes are needed only once the full dispatch has been
    /// compacted into a completion tombstone.
    ///
    /// A row stored before the Session published typed MCP credential
    /// requirements lacks that field, while a retry of the same command now
    /// carries it, derived from the same runtime envelope. Only then is the
    /// retry compared in the form that build would have written; the stored
    /// bytes are never rewritten and every other field must still be equal.
    #[must_use]
    pub fn same_canonical_dispatch(&self, other: &Self) -> bool {
        self.canonical_bytes() == other.canonical_bytes()
            || (self.placement.session_mcp_credentials.is_none()
                && other
                    .without_published_mcp_requirements()
                    .is_some_and(|legacy| self.canonical_bytes() == legacy.canonical_bytes()))
    }

    /// Compare an observed original request without an upgrade fallback. The
    /// Coordinator uses this only while atomically adopting that exact old row;
    /// trace context remains attempt-local under the canonical identity law.
    #[must_use]
    pub fn same_original_dispatch(&self, other: &Self) -> bool {
        self.canonical_bytes() == other.canonical_bytes()
    }

    /// Tombstone twin of the upgrade-window rule in
    /// [`Self::same_canonical_dispatch`]: the canonical fingerprint this retry
    /// would have had before the Session published typed MCP requirements, or
    /// `None` when it carries none. A completion tombstone written before that
    /// change matches only this form; one written since matches only
    /// [`Self::canonical_fingerprint`].
    #[must_use]
    pub fn pre_mcp_requirement_canonical_fingerprint(&self) -> Option<String> {
        self.without_published_mcp_requirements()
            .map(|legacy| legacy.canonical_fingerprint())
    }

    fn without_published_mcp_requirements(&self) -> Option<Self> {
        self.placement.session_mcp_credentials.as_ref()?;
        let mut legacy = self.clone();
        legacy.placement.session_mcp_credentials = None;
        Some(legacy)
    }

    /// The only non-root use of SessionCommand identity: a Task attention
    /// admitted under its existing parent Session activity. Every subject axis
    /// is carried in private input and bound by the original command FPR.
    /// Ordinary children never acquire this law from an incoming marker.
    #[must_use]
    pub fn is_task_child_command(&self) -> bool {
        let Some(tenure) = self
            .private_task_input
            .as_ref()
            .and_then(|input| input.tenure())
        else {
            return false;
        };
        matches!(&tenure.partition,
            awaken_session_application_contract::TaskReadPartition::Session { session_thread }
                if session_thread != &tenure.thread
                    && self.session_thread_id.as_ref().is_some_and(|id| &id.0 == session_thread))
            && tenure.validate().is_ok()
            && self.thread_id().0 == tenure.thread
            && self.session_activity_epoch.is_some_and(|epoch| epoch > 0)
            && self
                .session_command_fingerprint
                .as_ref()
                .is_some_and(|fp| fp.is_current())
    }

    /// Compare only the immutable Session command identity. This method is for
    /// the dedicated root-reservation port; ordinary and child Run admissions
    /// continue to use [`Self::same_canonical_dispatch`].
    #[must_use]
    pub fn same_session_reservation(&self, other: &Self) -> bool {
        if matches!(
            self.identity_scope,
            DispatchIdentityScope::PreparedSessionCommand
        ) || matches!(
            other.identity_scope,
            DispatchIdentityScope::PreparedSessionCommand
        ) {
            return self.identity_scope == other.identity_scope
                && self
                    .session_command_fingerprint
                    .as_ref()
                    .is_some_and(|stored| {
                        stored.is_current()
                            && other.session_command_fingerprint.as_ref() == Some(stored)
                    });
        }
        match self.session_command_fingerprint.as_ref() {
            Some(stored) if stored.is_current() => other
                .session_command_fingerprint
                .as_ref()
                .is_some_and(|incoming| incoming.is_current() && incoming == stored),
            Some(_) => false,
            None => {
                self.legacy_session_reservation_bytes() == other.legacy_session_reservation_bytes()
            }
        }
    }

    /// Compact identity retained after a Session root dispatch settles. It is
    /// the tombstone twin of [`Self::same_session_reservation`].
    #[must_use]
    pub fn session_reservation_fingerprint(&self) -> String {
        self.session_command_fingerprint.as_ref().map_or_else(
            || {
                if self.identity_scope == DispatchIdentityScope::PreparedSessionCommand {
                    // A corrupt prepared row has no legacy command identity.
                    // Retaining its complete bytes cannot create a reduced
                    // completion identity accepted by a current command.
                    self.canonical_fingerprint()
                } else {
                    self.legacy_session_reservation_fingerprint()
                }
            },
            |fingerprint| fingerprint.as_str().to_string(),
        )
    }

    /// Pre-versioned compact identity retained only for comparing a current
    /// incoming retry with a completion or live row written by an older build.
    /// Callers must choose it from stored legacy evidence, never from an
    /// incoming request.
    #[must_use]
    pub fn legacy_session_reservation_fingerprint(&self) -> String {
        let fingerprint = awaken_agent_contract::content_fingerprint(&(
            &self.activation.run_id,
            &self.activation.thread_id,
            &self.activation.input,
            &self.activation.delegation_origin,
            &self.activation.data_subject_id,
            &self.activation.tool_capability_narrowing(),
            &self.session_thread_id,
        ))
        .expect("legacy Session reservation identity has no fallible serializable value");
        format!("sha256:{fingerprint}")
    }

    /// Compare an incoming retry using the identity relation persisted by this
    /// stored dispatch. A Session reservation remains recognizable through the
    /// ordinary repair-claim surfaces even though their retry value does not
    /// author the stored scope; a generic stored dispatch never acquires relaxed
    /// identity from an incoming Session-shaped value.
    #[must_use]
    pub fn same_admission_dispatch(&self, other: &Self) -> bool {
        if self.identity_scope == DispatchIdentityScope::SessionCommand
            && self.is_task_child_command()
        {
            return other.identity_scope == DispatchIdentityScope::SessionCommand
                && other.is_task_child_command()
                && self.same_session_reservation(other);
        }
        match self.identity_scope {
            DispatchIdentityScope::SessionCommand
            | DispatchIdentityScope::PreparedSessionCommand
                if matches!(
                    other.admission_shape(),
                    DispatchAdmissionShape::SessionRootAwaitingActivity
                        | DispatchAdmissionShape::SessionRootWithActivity
                ) =>
            {
                self.same_session_reservation(other)
            }
            DispatchIdentityScope::SessionCommand
            | DispatchIdentityScope::PreparedSessionCommand
            | DispatchIdentityScope::FullDispatch => self.same_canonical_dispatch(other),
        }
    }

    /// Tombstone twin of [`Self::same_admission_dispatch`]. Live and completed
    /// replay can therefore never disagree about the accepted Run identity.
    #[must_use]
    pub fn admission_fingerprint(&self) -> String {
        match self.identity_scope {
            DispatchIdentityScope::SessionCommand
            | DispatchIdentityScope::PreparedSessionCommand => {
                self.session_reservation_fingerprint()
            }
            DispatchIdentityScope::FullDispatch => self.canonical_fingerprint(),
        }
    }

    /// Mark the request as owned by the dedicated Session reservation port.
    /// Storage adapters call this through their one shared validation function;
    /// generic enqueue/continuation callers cannot acquire the relaxed identity
    /// merely by constructing a self-affine routing shape.
    #[must_use]
    pub fn with_session_command_identity(mut self) -> Self {
        self.identity_scope = DispatchIdentityScope::SessionCommand;
        self
    }

    /// Mark a complete prepared activation as a distinct reservation format.
    /// Historical reduced Session identities cannot authorize its replay.
    #[must_use]
    pub fn with_prepared_session_command_identity(mut self) -> Self {
        self.identity_scope = DispatchIdentityScope::PreparedSessionCommand;
        self.bind_worker_codec_requirements();
        self
    }

    /// Native and ACP's owned-tool continuation consume ActiveToolBatch.
    /// A2A-only candidates do not; a Native/ACP fallback still does. This is
    /// the one frozen-candidate compiler used by placement and claim admission.
    pub fn requires_tool_result_journal(
        models: &awaken_runtime_contract::resolved::ResolvedSpec,
    ) -> bool {
        models.attempt_candidates(None).iter().any(|candidate| {
            matches!(
                awaken_runtime_contract::resolved::Backend::from_ref(
                    candidate.binding().identity().2
                ),
                awaken_runtime_contract::resolved::Backend::Native
                    | awaken_runtime_contract::resolved::Backend::Acp(_)
            )
        })
    }

    /// A Run whose frozen Agent enables BackgroundTask, or that carries Task
    /// observations, needs a Worker that decodes the private Task input and
    /// delivers Task attention. This is codec support, not Task tenure.
    pub fn requires_background_task_control(
        spec: &awaken_runtime_contract::resolved::ResolvedSpec,
    ) -> bool {
        spec.plugin_ids
            .iter()
            .any(|id| id == BACKGROUND_TASK_PLUGIN_ID)
    }

    /// The one Task-control demand used by wire eligibility and durable source
    /// retention. This observes frozen inputs, never a Worker capability grant.
    #[must_use]
    pub fn needs_background_task_control(&self) -> bool {
        self.private_task_input.is_some()
            || Self::requires_background_task_control(&self.activation.snapshot.resolved_spec)
    }

    fn required_worker_codecs(
        &self,
        include_runtime_journal: bool,
    ) -> impl Iterator<Item = &'static str> + use<> {
        let prepared = (self.identity_scope == DispatchIdentityScope::PreparedSessionCommand)
            .then_some(awaken_worker_contract::PREPARED_SESSION_DISPATCH_CAPABILITY);
        let journal = (include_runtime_journal
            && Self::requires_tool_result_journal(&self.activation.snapshot.resolved_spec))
        .then_some(awaken_worker_contract::TOOL_RESULT_JOURNAL_CAPABILITY);
        let task_control = self
            .needs_background_task_control()
            .then_some(awaken_worker_contract::BACKGROUND_TASK_CONTROL_CAPABILITY);
        [prepared, journal, task_control].into_iter().flatten()
    }

    fn bind_worker_codec_requirements(&mut self) {
        if self.identity_scope != DispatchIdentityScope::PreparedSessionCommand {
            return;
        }
        let required = self.required_worker_codecs(true);
        // This narrows a registered Worker's eligibility but does not force an
        // already installed local owner to acquire a registered Worker.
        self.placement
            .required_capabilities
            .extend(required.map(str::to_owned));
    }

    /// Publish the intrinsic codec requirements when a caller deliberately
    /// constructs a Worker-bound request. Legacy/default constructors retain
    /// their exact wire bytes; this never changes execution location.
    #[must_use]
    pub fn with_worker_codec_requirements(mut self) -> Self {
        let required = self.required_worker_codecs(true);
        self.placement
            .required_capabilities
            .extend(required.map(str::to_owned));
        self
    }

    /// Canonical submission refuses a caller that stripped mandatory codec
    /// requirements. The closed request kind, not optional caller input,
    /// determines which formats a Worker must understand.
    pub fn validate_worker_codec_requirements(
        &self,
    ) -> Result<(), awaken_worker_contract::Incompatibility> {
        let remote_wire = self.placement.location
            != awaken_worker_contract::ExecutionLocation::LocalOnly
            && self.placement.runtime_protocol_version != 0;
        awaken_worker_contract::admit_required_capabilities(
            self.required_worker_codecs(remote_wire),
            &self.placement.required_capabilities,
        )
    }

    /// A registered Worker cannot turn an old unmarked request into a new
    /// journal producer, even when its own binary supports the new format.
    /// An absent immutable manifest proves no offered codec capabilities.
    pub fn validate_worker_wire_codec_requirements(
        &self,
        worker: Option<&awaken_worker_contract::WorkerManifest>,
    ) -> Result<(), awaken_worker_contract::Incompatibility> {
        awaken_worker_contract::admit_required_capabilities(
            self.required_worker_codecs(true),
            &self.placement.required_capabilities,
        )?;
        let no_offers = std::collections::BTreeSet::new();
        awaken_worker_contract::admit_required_capabilities(
            self.required_worker_codecs(true),
            worker.map_or(&no_offers, |manifest| &manifest.capabilities),
        )
    }

    /// Every Worker claim mode returns the same full request, including
    /// reservation repair and cancellation. Those modes may bypass execution
    /// placement, but must never bypass the same mandatory capability rule.
    /// None is the existing in-process owner, which opens no Worker wire hop.
    pub fn worker_codec_admission(
        &self,
        worker: Option<&awaken_worker_contract::WorkerManifest>,
    ) -> Result<(), awaken_worker_contract::Incompatibility> {
        match worker {
            Some(manifest) => self.validate_worker_wire_codec_requirements(Some(manifest)),
            None => self.validate_worker_codec_requirements(),
        }
    }

    /// Attach the Session authority's command identity. The reservation port
    /// separately marks the accepted row's identity scope after validation.
    #[must_use]
    pub fn with_session_command_fingerprint(
        mut self,
        fingerprint: SessionRunCommandFingerprint,
    ) -> Self {
        self.session_command_fingerprint = Some(fingerprint);
        self
    }

    /// Route execution through an existing session without changing the Run's
    /// own thread identity used by lifecycle and queue single-writer rules.
    pub fn for_session(mut self, thread_id: ThreadId) -> Self {
        self.session_thread_id = Some(thread_id);
        self
    }

    /// Bind a coordinated child to the Session activity admitted for its stable
    /// operation identity.
    #[must_use]
    pub fn with_session_activity_epoch(mut self, epoch: u64) -> Self {
        self.session_activity_epoch = Some(epoch);
        self
    }

    /// Preserve the Session application's explicit replacement intent across
    /// reservation recovery.
    #[must_use]
    pub fn with_session_run_replacement(mut self, replacement: SessionRunReplacement) -> Self {
        self.session_run_replacement = replacement;
        self
    }

    /// Attach the admitting request's W3C `traceparent` (see the field docs).
    pub fn with_traceparent(mut self, traceparent: Option<String>) -> Self {
        self.traceparent = traceparent;
        self
    }

    /// Attach the verified scope's durable opaque representation.
    #[must_use]
    pub fn with_execution_scope(mut self, scope: ExecutionScopeRef) -> Self {
        self.execution_scope = Some(scope);
        self
    }

    /// Attach the exact resource manifest selected by Session creation.
    #[must_use]
    pub fn with_session_resources(mut self, resources: SessionResourceEnvelope) -> Self {
        self.session_resources = Some(resources);
        self
    }

    /// Attach the exact runtime projection frozen at Session creation.
    #[must_use]
    pub fn with_session_runtime(mut self, runtime: SessionRuntimeEnvelope) -> Self {
        self.session_runtime = Some(runtime);
        self
    }

    /// Attach the exact non-root execution-publication closure frozen at admission.
    #[must_use]
    pub fn with_agent_publications(
        mut self,
        publications: Vec<awaken_runtime_contract::ExecutableAgentSnapshot>,
    ) -> Self {
        self.agent_publications = publications;
        self
    }

    #[must_use]
    pub fn with_preferred_environment_shape(mut self, shape: SandboxCapacityShapeId) -> Self {
        self.preferred_environment_shape = Some(shape);
        self
    }

    /// Attach the trusted Environment/deployment's exact inference holder
    /// request. This is one request, never a preference list or fallback order.
    #[must_use]
    pub fn with_inference_plaintext_holder(mut self, holder: PlaintextHolder) -> Self {
        self.inference_plaintext_holder = Some(holder);
        self
    }

    /// Pin the immutable worker-placement requirements carried through every
    /// crash recovery and replacement attempt.
    #[must_use]
    pub fn with_placement(mut self, placement: PlacementRequirements) -> Self {
        self.placement = placement;
        self.bind_worker_codec_requirements();
        self
    }

    pub fn run_id(&self) -> &RunId {
        &self.activation.run_id
    }

    pub fn thread_id(&self) -> &ThreadId {
        &self.activation.thread_id
    }

    pub fn session_thread_id(&self) -> &ThreadId {
        self.session_thread_id
            .as_ref()
            .unwrap_or(&self.activation.thread_id)
    }
}

#[cfg(kani)]
/// @obligation delegation.independent_child_scheduling_lease
/// @proof-input-debt free_symbol classify_dispatch_admission has_session_thread
/// @proof-input-debt free_symbol classify_dispatch_admission session_thread_matches
#[kani::proof]
fn dispatch_admission_shape_keeps_child_affinity_independent() {
    // Cause/effect graph: C1 Session affinity is absent, matches the Run Thread,
    // or names a distinct parent Thread; C2 activity is absent, positive, or
    // zero. Effects: E1 ordinary root, E2 root awaiting activity, E3 root with
    // activity, E4 independently scheduled Session child, E5 invalid zero.
    // Constraint: this descriptive reducer never authors identity or lifecycle.
    // Decision rules: R1 absent+valid=>E1; R2 same+absent=>E2;
    // R3 same+positive=>E3; R4 distinct+absent/positive=>E4; R5 any+zero=>E5.
    let has_session_thread: bool = kani::any();
    let session_thread_matches: bool = kani::any();
    let has_activity: bool = kani::any();
    let activity = has_activity.then_some(kani::any::<u64>());
    let result = classify_dispatch_admission(has_session_thread, session_thread_matches, activity);

    if activity == Some(0) {
        assert_eq!(
            result,
            DispatchAdmissionShape::InvalidZeroActivityEpoch,
            "R5/E5"
        );
    } else if !has_session_thread {
        assert_eq!(result, DispatchAdmissionShape::OrdinaryRoot, "R1/E1");
    } else if session_thread_matches && activity.is_none() {
        assert_eq!(
            result,
            DispatchAdmissionShape::SessionRootAwaitingActivity,
            "R2/E2"
        );
    } else if session_thread_matches {
        assert_eq!(
            result,
            DispatchAdmissionShape::SessionRootWithActivity,
            "R3/E3"
        );
    } else {
        assert_eq!(result, DispatchAdmissionShape::SessionChild, "R4/E4");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::RedactedString;
    use awaken_session_configuration_contract::resource::SessionResourceManifest;
    use awaken_session_configuration_contract::{
        EnvironmentSnapshot, SessionRuntimeProjection, SessionToolConfiguration,
    };

    #[test]
    fn dispatch_admission_shape_is_total_and_port_neutral() {
        // Cause/effect graph: C1 Session affinity is absent/self/other; C2 the
        // activity coordinate is absent/positive/zero. Effects: E1 ordinary
        // root; E2 root intent awaiting Session activity; E3 activity-bound root;
        // E4 child; E5 explicit invalid-zero evidence. Constraint: classification
        // persists nothing and grants no admission authority. Decision table:
        // R1=!C1+any valid -> E1; R2=self+absent -> E2; R3=self+positive -> E3;
        // R4=other+absent/positive -> E4; R5=any+zero -> E5.
        let ordinary = RunDispatch::new(activation());
        assert_eq!(
            ordinary.admission_shape(),
            DispatchAdmissionShape::OrdinaryRoot,
            "R1/E1"
        );
        let session = ordinary.thread_id().clone();
        let intent = ordinary.clone().for_session(session.clone());
        assert_eq!(
            intent.admission_shape(),
            DispatchAdmissionShape::SessionRootAwaitingActivity,
            "R2/E2"
        );
        assert_eq!(
            intent
                .clone()
                .with_session_activity_epoch(7)
                .admission_shape(),
            DispatchAdmissionShape::SessionRootWithActivity,
            "R3/E3"
        );
        assert_eq!(
            ordinary
                .clone()
                .for_session(ThreadId("session-parent".into()))
                .admission_shape(),
            DispatchAdmissionShape::SessionChild,
            "R4/E4"
        );
        assert_eq!(
            intent.with_session_activity_epoch(0).admission_shape(),
            DispatchAdmissionShape::InvalidZeroActivityEpoch,
            "R5/E5"
        );
    }

    use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
    use awaken_agent_contract::credential::CredentialPurpose;
    use awaken_agent_contract::credential::{
        CredentialAccess, CredentialEnvelope, CredentialExecutionPolicy, CredentialMaterialSource,
        CredentialRef, CredentialTarget, CredentialUsage, SealedCredentialEnvelopeRef,
        TrustDomainRef,
    };
    use awaken_credential_contract::CredentialMaterial;
    use awaken_runtime_contract::resolved::{
        CatalogFingerprint, ModelBinding, ModelProvisioning, ResolvedSpec,
    };
    use awaken_runtime_contract::snapshot::{
        AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
    };

    fn provider_candidate(
        model: &str,
        credential: &str,
        provider: &str,
        route: &str,
    ) -> awaken_runtime_contract::resolved::ResolvedModelCandidate {
        provider_candidate_with_access(
            model,
            credential,
            provider,
            route,
            CredentialMaterialSource::ControlPlaneReference,
            None,
        )
    }

    fn provider_candidate_with_access(
        model: &str,
        credential: &str,
        provider: &str,
        route: &str,
        material_source: CredentialMaterialSource,
        envelope: Option<CredentialEnvelope>,
    ) -> awaken_runtime_contract::resolved::ResolvedModelCandidate {
        let mut access = CredentialAccess::new(
            CredentialRef {
                id: credential.into(),
                revision: 0,
            },
            material_source,
            CredentialUsage::ProviderAdapter,
            CredentialExecutionPolicy::self_hosted_provider(),
        )
        .with_target(CredentialTarget::new(
            CredentialPurpose::ProviderAdapter,
            provider.split_once('@').map_or(provider, |(id, _)| id),
        ));
        if let Some(envelope) = envelope {
            access = access.with_envelope(envelope);
        }
        awaken_runtime_contract::resolved::ResolvedModelCandidate::try_provider(
            ModelBinding::new(provider, model, "genai"),
            provider,
            route,
            "workspace-a",
            Some(access),
            awaken_runtime_contract::InferenceEndpoint {
                adapter_kind: "openai".into(),
                api_dialect: "open_ai_chat".into(),
                base_url: "https://provider.invalid/v1".into(),
                upstream_model: model.into(),
                processing_placement: None,
            },
        )
        .expect("coherent run-dispatch provider candidate")
    }

    fn activation() -> RunActivation {
        RunActivation::new(
            RunId("run-1".into()),
            ThreadId("thrd-1".into()),
            ExecutableAgentSnapshot {
                id: ExecutableAgentSnapshotId("snap".into()),
                metadata: Default::default(),
                root_agent_id: AgentId("agent".into()),
                resolved_spec: ResolvedSpec {
                    model_candidates: Vec::new(),
                    catalog_fingerprint: CatalogFingerprint("fp".into()),
                    instructions: "be helpful".into(),
                    max_steps: Some(8),
                    delegation_limits: Default::default(),
                    model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                        ModelBinding::new("prov", "model", "acp:test"),
                    ),
                    tool_descriptors: Vec::new(),
                    plugin_ids: Vec::new(),
                    plugin_config: Default::default(),
                    context_policy: Default::default(),
                    tool_presentation: Default::default(),
                },
                fingerprint: CatalogFingerprint("fp".into()),
            },
            vec![Message::text(MessageId("u1".into()), Role::User, "go")],
        )
    }

    fn command_fingerprint(fill: char) -> SessionRunCommandFingerprint {
        serde_json::from_value(serde_json::Value::String(format!(
            "{}{}",
            SessionRunCommandFingerprint::CURRENT_PREFIX,
            fill.to_string().repeat(64)
        )))
        .expect("test fingerprint decodes")
    }

    /// The request a durable queue persists and replays must survive a
    /// serialize→deserialize round-trip unchanged — G3's whole point (it carries no
    /// live handle), and the accessors read the same ids back out.
    #[test]
    fn round_trips_through_serde_with_its_accessors_intact() {
        let req = RunDispatch::new(activation()).with_traceparent(Some("00-abc-01".into()));
        let json = serde_json::to_string(&req).expect("serializes");
        let back: RunDispatch = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(back, req, "round-trip is lossless");
        assert_eq!(back.run_id(), req.run_id());
        assert_eq!(back.thread_id(), req.thread_id());
        assert_eq!(back.traceparent.as_deref(), Some("00-abc-01"));
    }

    #[test]
    fn coordinated_activity_epoch_has_one_backward_compatible_dispatch_field() {
        // Cause/effect graph: C1 a coordinated child carries an admitted
        // Session activity epoch; C2 a legacy durable row omits the field; C3
        // only the queue-owned epoch changes; C4 execution payload changes.
        // Effects: E1 C1 round-trips the exact epoch; E2 C2 decodes as ordinary
        // `None`; E3 the optional field is omitted for ordinary rows; E4 C3 is
        // the same Run identity because Awaiting reply admission rotates it; E5
        // C4 remains a collision.
        //
        // | Rule | Serialized field | Change | Effects |
        // | R1 | present | none | E1 |
        // | R2 | absent | none | E2+E3 |
        // | R3 | present | activity epoch | E4 |
        // | R4 | present | instructions | E5 |
        // Constraint/Invariant: the queue-owned activity epoch is compatibility
        // metadata, never a second Run identity input. Decision rule: R1-R4
        // cover present/absent wire shape and metadata-only/payload changes.
        let coordinated = RunDispatch::new(activation()).with_session_activity_epoch(17);
        let encoded = serde_json::to_value(&coordinated).expect("R1 serializes");
        assert_eq!(encoded["session_activity_epoch"], 17, "R1/E1");
        let decoded: RunDispatch = serde_json::from_value(encoded).expect("R1 decodes");
        assert_eq!(decoded.session_activity_epoch, Some(17), "R1/E1");

        let ordinary = RunDispatch::new(activation());
        let legacy = serde_json::to_value(&ordinary).expect("R2 serializes");
        assert!(legacy.get("session_activity_epoch").is_none(), "R2/E3");
        let decoded: RunDispatch = serde_json::from_value(legacy).expect("R2 legacy decodes");
        assert_eq!(decoded.session_activity_epoch, None, "R2/E2");

        let rotated = coordinated.clone().with_session_activity_epoch(18);
        assert!(coordinated.same_canonical_dispatch(&rotated), "R3/E4");
        assert_eq!(
            coordinated.canonical_fingerprint(),
            rotated.canonical_fingerprint(),
            "R3/E4"
        );
        let mut changed_execution = coordinated.clone();
        changed_execution
            .activation
            .snapshot
            .resolved_spec
            .instructions = "changed".to_string();
        assert!(
            !coordinated.same_canonical_dispatch(&changed_execution),
            "R4/E5"
        );
    }

    #[test]
    fn live_and_tombstone_identity_share_canonical_bytes() {
        // Identity decision rule I1: C1 two dispatch values are Rust-equal but
        // their serialized execution payload differs (`-0.0` versus `0.0`);
        // E1 live comparison rejects them and E2 tombstone fingerprints differ.
        // I2: C2 only traceparent differs => E3 both comparisons replay. This
        // locks live and compacted identity to one byte equivalence relation.
        let mut negative_zero = RunDispatch::new(activation());
        negative_zero
            .activation
            .snapshot
            .resolved_spec
            .plugin_config
            .insert("number".into(), serde_json::json!(-0.0));
        let mut positive_zero = negative_zero.clone();
        positive_zero
            .activation
            .snapshot
            .resolved_spec
            .plugin_config
            .insert("number".into(), serde_json::json!(0.0));
        assert_eq!(negative_zero, positive_zero, "I1/C1 precondition");
        assert!(
            !negative_zero.same_canonical_dispatch(&positive_zero),
            "I1/E1"
        );
        assert_ne!(
            negative_zero.canonical_fingerprint(),
            positive_zero.canonical_fingerprint(),
            "I1/E2"
        );

        let retraced = negative_zero.clone().with_traceparent(Some(
            "00-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbbbbbbbbbb-01".into(),
        ));
        assert!(negative_zero.same_canonical_dispatch(&retraced), "I2/E3");
        assert_eq!(
            negative_zero.canonical_fingerprint(),
            retraced.canonical_fingerprint(),
            "I2/E3"
        );
    }

    #[test]
    fn session_reservation_identity_separates_command_from_resolved_execution() {
        // Cause/effect graph: C1 immutable Session command fields are exact or
        // changed; C2 current execution projections are exact or changed; C3
        // the dispatch is a self-affine Session root or ordinary root. Effects:
        // E1 C1 exact+C2 changed replays the reservation and retains one compact
        // identity; E2 C1 changed conflicts; E3 ordinary completion identity
        // remains the full canonical dispatch. Constraint: comparison never
        // overwrites the first accepted request. Decision rules:
        // R1=exact command+changed projection+Session=>E1;
        // R2=changed command+any projection+Session=>E2;
        // R3=changed projection+ordinary=>E3.
        let session = ThreadId("thrd-1".into());
        let original = RunDispatch::new(activation())
            .for_session(session.clone())
            .with_session_command_fingerprint(command_fingerprint('a'))
            .with_session_command_identity();

        let mut reprojected = original.clone();
        reprojected.activation.snapshot.resolved_spec.instructions = "current publication".into();
        reprojected.activation.model_ref_override = Some("current-route".into());
        reprojected.execution_scope = Some(ExecutionScopeRef(
            awaken_agent_contract::ScopeId::from("current-workspace"),
        ));
        reprojected.session_resources = Some(SessionResourceEnvelope::at_revision(
            "current-workspace",
            9,
            r#"{"inputs":[]}"#,
        ));
        reprojected.placement = PlacementRequirements::remote_required();
        assert!(original.same_session_reservation(&reprojected), "R1/E1");
        assert_eq!(
            original.session_reservation_fingerprint(),
            reprojected.session_reservation_fingerprint(),
            "R1/E1"
        );
        assert_ne!(
            original.canonical_fingerprint(),
            reprojected.canonical_fingerprint(),
            "R1 precondition"
        );

        let mut changed_command = reprojected.clone();
        changed_command.activation.input = vec![Message::text(
            MessageId("u1".into()),
            Role::User,
            "different command",
        )];
        changed_command.session_command_fingerprint = Some(command_fingerprint('b'));
        assert!(
            !original.same_session_reservation(&changed_command),
            "R2/E2"
        );
        assert_ne!(
            original.session_reservation_fingerprint(),
            changed_command.session_reservation_fingerprint(),
            "R2/E2"
        );

        let ordinary = RunDispatch::new(activation());
        let mut ordinary_reprojected = ordinary.clone();
        ordinary_reprojected
            .activation
            .snapshot
            .resolved_spec
            .instructions = "changed".into();
        assert_ne!(
            ordinary.admission_fingerprint(),
            ordinary_reprojected.admission_fingerprint(),
            "R3/E3"
        );
        assert_eq!(
            original.admission_fingerprint(),
            original.session_reservation_fingerprint(),
            "R1/E1"
        );
    }

    #[test]
    fn session_command_identity_wire_and_legacy_selection_follow_stored_evidence() {
        // Cause/effect graph: C1 stored identity is current, absent legacy, or
        // unknown; C2 incoming identity is exact current, changed current, or
        // absent. Effects: E1 current round-trips and exact replays; E2 changed
        // current conflicts; E3 only a stored absence selects legacy subset
        // comparison; E4 unknown stored and current-stored/missing-incoming fail
        // closed. Decision rules W1=current+exact=>E1,
        // W2=current+changed=>E2, W3=absent+legacy-equivalent=>E3,
        // W4=unknown|current+missing=>E4.
        let current = RunDispatch::new(activation())
            .for_session(ThreadId("thrd-1".into()))
            .with_session_command_fingerprint(command_fingerprint('a'))
            .with_session_command_identity();
        let wire = serde_json::to_value(&current).expect("W1 serializes");
        let round_trip: RunDispatch = serde_json::from_value(wire).expect("W1 deserializes");
        assert_eq!(round_trip, current, "W1/E1");
        assert!(current.same_session_reservation(&round_trip), "W1/E1");
        assert!(
            current
                .session_reservation_fingerprint()
                .starts_with(SessionRunCommandFingerprint::CURRENT_PREFIX),
            "W1/E1"
        );

        let changed = current
            .clone()
            .with_session_command_fingerprint(command_fingerprint('b'));
        assert!(!current.same_session_reservation(&changed), "W2/E2");

        let legacy = RunDispatch::new(activation())
            .for_session(ThreadId("thrd-1".into()))
            .with_session_command_identity();
        let legacy_wire = serde_json::to_value(&legacy).expect("W3 serializes");
        assert!(
            legacy_wire.get("session_command_fingerprint").is_none(),
            "W3/E3"
        );
        assert!(legacy.same_session_reservation(&current), "W3/E3");
        assert!(
            !current.same_session_reservation(&legacy),
            "W4/E4 stored current does not select legacy"
        );

        let unknown = serde_json::from_value(serde_json::Value::String(
            "session-command-v2:sha256:future".into(),
        ))
        .expect("W4 unknown value remains inspectable");
        let unknown = legacy.clone().with_session_command_fingerprint(unknown);
        assert!(!unknown.same_session_reservation(&current), "W4/E4");

        // P1 complete prepared input has its own closed identity kind and exact
        // current fingerprint; P2 ordinary current/legacy/unknown identities
        // cannot be upgraded by an incoming prepared request. P3 old readers
        // fail closed on the new kind, while ordinary wire omission is intact.
        let prepared = current.clone().with_prepared_session_command_identity();
        assert!(prepared.same_session_reservation(&prepared), "P1");
        for other in [&current, &legacy, &unknown] {
            assert!(!other.same_session_reservation(&prepared), "P2");
            assert!(!prepared.same_session_reservation(other), "P2");
        }
        let mut missing_prepared = prepared.clone();
        missing_prepared.session_command_fingerprint = None;
        assert!(!missing_prepared.same_session_reservation(&prepared), "P2");
        assert!(!prepared.same_session_reservation(&missing_prepared), "P2");
        let prepared_wire = serde_json::to_value(&prepared).unwrap();
        assert_eq!(
            prepared_wire["identity_scope"], "prepared_session_command",
            "P3"
        );
        assert_eq!(
            serde_json::from_value::<RunDispatch>(prepared_wire.clone()).unwrap(),
            prepared,
            "P1"
        );
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum OldIdentityScope {
            FullDispatch,
            SessionCommand,
        }
        assert!(
            serde_json::from_value::<OldIdentityScope>(prepared_wire["identity_scope"].clone())
                .is_err(),
            "P3"
        );
        assert!(
            serde_json::to_value(RunDispatch::new(activation()))
                .unwrap()
                .get("identity_scope")
                .is_none(),
            "P3 ordinary unchanged"
        );
    }

    #[test]
    fn background_task_rows_demand_task_control_and_refuse_other_input_versions() {
        use awaken_worker_contract::BACKGROUND_TASK_CONTROL_CAPABILITY as TASK;
        // C1: a row whose snapshot enables BackgroundTask, or that carries
        // private Task input, demands the Task control codec; a plain row does
        // not. C2/C3: the carried input round-trips at version 1, a row with
        // any other input version does not decode, and a row without input
        // keeps its exact bytes (no `private_task_input` key).
        let worker_bound = |row: RunDispatch| {
            row.with_placement(PlacementRequirements::remote_preferred())
                .with_worker_codec_requirements()
        };
        let plain = worker_bound(RunDispatch::new(activation()));
        assert!(
            !plain.placement.required_capabilities.contains(TASK),
            "C1 plain"
        );
        assert!(
            serde_json::to_value(&plain)
                .unwrap()
                .get("private_task_input")
                .is_none(),
            "C2 absent input is omitted"
        );

        let mut enabled = activation();
        enabled
            .snapshot
            .resolved_spec
            .plugin_ids
            .push(BACKGROUND_TASK_PLUGIN_ID.into());
        assert!(RunDispatch::requires_background_task_control(
            &enabled.snapshot.resolved_spec
        ));
        let enabled = worker_bound(RunDispatch::new(enabled));
        assert!(
            enabled.placement.required_capabilities.contains(TASK),
            "C1 plugin"
        );

        let mut carried = RunDispatch::new(activation());
        let input = awaken_session_application_contract::PrivateTaskInput::new(vec![
            serde_json::json!({"version": 1}),
        ])
        .unwrap();
        carried.private_task_input = Some(input.clone());
        let carried = worker_bound(carried);
        assert!(
            carried.placement.required_capabilities.contains(TASK),
            "C1 input"
        );
        let mut stripped = carried.clone();
        stripped.placement.required_capabilities.remove(TASK);
        assert!(
            stripped.validate_worker_codec_requirements().is_err(),
            "C1 a stripped demand is refused"
        );

        let wire = serde_json::to_value(&carried).unwrap();
        assert_eq!(wire["private_task_input"]["version"], 1, "C2");
        let decoded: RunDispatch = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(decoded.private_task_input, Some(input), "C2");
        for version in [
            serde_json::json!(0),
            serde_json::json!(2),
            serde_json::Value::Null,
        ] {
            let mut historical = wire.clone();
            historical["private_task_input"]["version"] = version;
            assert!(
                serde_json::from_value::<RunDispatch>(historical).is_err(),
                "C3 other input version"
            );
        }
    }

    #[test]
    fn worker_codec_requirements_are_intrinsic_and_local_legacy_bytes_stay_unchanged() {
        use awaken_worker_contract::{
            PREPARED_SESSION_DISPATCH_CAPABILITY as PREPARED,
            TOOL_RESULT_JOURNAL_CAPABILITY as JOURNAL, VersionRange, WorkerManifest,
        };
        // Causes C1 prepared/ordinary kind; C2 Native/ACP/A2A primary;
        // C3 old explicit-v5 Worker omits prepared support versus actual new
        // support; C4 required metadata stripped; C5 no-Worker protocol0;
        // C6 remote transport has no immutable offered-capability evidence.
        // Effects: E1 C1 prepared always requires its positive codec token;
        // E2 Native/ACP also require persisted journal support; E3 C4 cannot
        // bypass even with an offered token or a local fallback on remote
        // input; E4 C5 legacy bytes/hash and local admission are unchanged.
        // Rules R1..R3 cross backend x prepared x offered; R4 strips metadata;
        // R5 is the explicit local/no-wire exception. R6 C6 admits only the
        // codec-free ordinary A2A row: unknown offers cannot certify a required
        // codec. Fallback membership is checked by placement's shared table.
        for backend in ["native", "acp:claude", "a2a:https://codec.invalid"] {
            let mut activation = activation();
            activation.snapshot.resolved_spec.model_binding = if backend.starts_with("a2a:") {
                awaken_runtime_contract::resolved::ResolvedModelCandidate::try_remote(
                    awaken_runtime_contract::resolved::ModelBinding::parse("", "", backend)
                        .expect("external fixture binding is admissible"),
                    "workspace",
                    None,
                    "sha256:codec-card",
                )
                .unwrap()
            } else {
                awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                    awaken_runtime_contract::resolved::ModelBinding::parse(
                        "provider", "model", backend,
                    )
                    .expect("complete fixture binding is admissible"),
                )
            };
            let local = RunDispatch::new(activation.clone());
            let old_bytes = serde_json::to_vec(&local).unwrap();
            let old_hash = local.canonical_fingerprint();
            assert!(
                local.worker_codec_admission(None).is_ok(),
                "R5 no Worker needed"
            );
            let decoded: RunDispatch = serde_json::from_slice(&old_bytes).unwrap();
            assert_eq!(
                serde_json::to_vec(&decoded).unwrap(),
                old_bytes,
                "R5 ordinary None wire"
            );
            assert_eq!(
                decoded.canonical_fingerprint(),
                old_hash,
                "R5 ordinary hash"
            );
            for prepared in [false, true] {
                let mut row = RunDispatch::new(activation.clone())
                    .with_placement(PlacementRequirements::remote_preferred())
                    .with_worker_codec_requirements();
                if prepared {
                    row = row.with_prepared_session_command_identity();
                }
                assert_eq!(
                    row.placement.required_capabilities.contains(PREPARED),
                    prepared,
                    "R1 intrinsic kind"
                );
                assert_eq!(
                    row.placement.required_capabilities.contains(JOURNAL),
                    !backend.starts_with("a2a:"),
                    "R2 intrinsic frozen backend"
                );
                assert_eq!(
                    row.validate_worker_wire_codec_requirements(None).is_ok(),
                    !prepared && backend.starts_with("a2a:"),
                    "R6 unknown remote offers cannot certify required codecs"
                );
                let mut old = WorkerManifest {
                    runtime_protocol: VersionRange::exact(
                        awaken_worker_contract::CURRENT_RUNTIME_PROTOCOL_VERSION,
                    ),
                    ..Default::default()
                };
                old.capabilities.insert(JOURNAL.into());
                assert_eq!(
                    row.worker_codec_admission(Some(&old)).is_ok(),
                    !prepared,
                    "R3 v5 alone cannot decode prepared"
                );
                old.capabilities.insert(PREPARED.into());
                assert!(
                    row.worker_codec_admission(Some(&old)).is_ok(),
                    "R3 positive declared support"
                );
                if prepared {
                    row.placement.required_capabilities.remove(PREPARED);
                } else if !backend.starts_with("a2a:") {
                    row.placement.required_capabilities.remove(JOURNAL);
                }
                if prepared || !backend.starts_with("a2a:") {
                    assert!(
                        row.validate_worker_codec_requirements().is_err(),
                        "R4 missing metadata"
                    );
                    assert!(
                        row.worker_codec_admission(Some(&old)).is_err(),
                        "R4 offered alone cannot upgrade row"
                    );
                    assert!(
                        row.worker_codec_admission(None).is_err(),
                        "R4 remote row cannot use local fallback"
                    );
                }
            }
        }
    }

    /// Resource-envelope compatibility causes/effects: C1 current dispatch has a
    /// Session generation; C2 legacy dispatch omits it. E1 C1 round-trips the
    /// exact generation; E2 C2 decodes as generation zero without dead-lettering.
    #[test]
    fn frozen_session_resources_round_trip_with_their_matching_scope() {
        let resources =
            SessionResourceEnvelope::at_revision("workspace-a", 4, r#"{"inputs":[],"skills":[]}"#);
        let request = RunDispatch::new(activation())
            .with_execution_scope(ExecutionScopeRef(awaken_agent_contract::ScopeId::from(
                "workspace-a",
            )))
            .with_session_resources(resources.clone());

        let json = serde_json::to_string(&request).expect("serializes");
        let recovered: RunDispatch = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(recovered.session_resources, Some(resources));
        assert_eq!(
            recovered.execution_scope,
            Some(ExecutionScopeRef(awaken_agent_contract::ScopeId::from(
                "workspace-a"
            )))
        );

        let legacy: SessionResourceEnvelope = serde_json::from_value(serde_json::json!({
            "workspace_id": "workspace-a",
            "resolved_resources_json": "{\"inputs\":[]}"
        }))
        .expect("legacy envelope");
        assert_eq!(legacy.resource_revision, 0);
    }

    /// Projection-ownership causes/effects: C1 current code constructs the
    /// contract-owned projection; C2 a durable dispatch serializes it. Effects:
    /// E1 the environment and explicit empty policy sets decode exactly; E2 the
    /// opaque envelope remains byte-stable through queue serialization. Rule P1
    /// covers C1=>E1 and P2 covers C1+C2=>E2, preventing Runtime Host from
    /// regaining a parallel private schema.
    #[test]
    fn frozen_session_runtime_projection_round_trips_through_its_contract_owner() {
        let environment = EnvironmentSnapshot {
            environment_id: "environment-a".into(),
            revision: awaken_environment_contract::EnvironmentRevision(1),
            self_hosted: true,
            config_fingerprint: awaken_session_configuration_contract::EnvironmentFingerprint(
                "environment-a@1".into(),
            ),
            sandbox: Default::default(),
            sandbox_provisioning: Default::default(),
            idle_retention: Default::default(),
            packages: Default::default(),
            prepared_image: None,
            network: awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
            credential_realization:
                awaken_credential_contract::CredentialRealizationProfile::self_hosted_native(),
        };
        let tools = SessionToolConfiguration {
            toolsets: Vec::new(),
            client_tools: vec![awaken_agent_contract::ClientToolDescriptor {
                name: "review_plan".into(),
                description: "Review the exact plan revision".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {"revision": {"type": "integer"}},
                    "required": ["revision"]
                }),
            }],
        };
        let runtime = SessionRuntimeEnvelope::new(
            SessionRuntimeProjection {
                environment: environment.clone(),
                tools: Some(tools.clone()),
                mcp_stages: Some(Vec::new()),
            }
            .encode()
            .expect("P1 encode"),
        );
        let projection =
            SessionRuntimeProjection::decode(&runtime.projection_json).expect("P1 decode");
        assert_eq!(projection.environment, environment, "P1/E1");
        assert_eq!(projection.tools, Some(tools), "P1/E1");
        assert_eq!(projection.mcp_stages, Some(Vec::new()), "P1/E1");

        let request = RunDispatch::new(activation()).with_session_runtime(runtime.clone());

        let json = serde_json::to_string(&request).expect("serializes");
        let recovered: RunDispatch = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(recovered.session_runtime, Some(runtime), "P2/E2");
    }

    /// Execution-publication transport cause/effect and FMECA design. Causes:
    /// C1 the parent activation has a non-root executable publication; C2 a
    /// legacy row omits the bundle. Effects: E1 the exact immutable dependency
    /// snapshot survives queue serialization; E2 C2 remains readable as empty.
    /// Rules P1=C1=>E1, P2=!C1+C2=>E2. FMECA: losing the bundle (high severity,
    /// cold Worker cannot create `agent_run`) is detected by P1; older rows have
    /// no invented authority and fail closed later when delegation is attempted.
    #[test]
    fn execution_publications_round_trip_and_legacy_rows_default_empty() {
        let child = ExecutableAgentSnapshot::builder("researcher")
            .model(awaken_runtime_contract::resolved::ModelBinding::new(
                "test", "model", "native",
            ))
            .instructions("research")
            .fingerprint("researcher-v1")
            .build();
        let request = RunDispatch::new(activation()).with_agent_publications(vec![child.clone()]);
        let wire = serde_json::to_value(&request).expect("P1 serialize");
        let restored: RunDispatch = serde_json::from_value(wire).expect("P1 deserialize");
        assert_eq!(restored.agent_publications, vec![child], "P1/E1");

        let legacy_wire = serde_json::to_value(RunDispatch::new(activation())).unwrap();
        let restored: RunDispatch = serde_json::from_value(legacy_wire).expect("P2 legacy");
        assert!(restored.agent_publications.is_empty(), "P2/E2");
    }

    /// A `None` traceparent is omitted on the wire (`skip_serializing_if`), so a row
    /// written by an older writer (no trace) is byte-identical and deserializes back
    /// to `None` rather than dead-lettering — the documented forward/back-compat
    /// guarantee.
    #[test]
    fn a_none_traceparent_is_omitted_and_a_legacy_row_loads_as_none() {
        let req = RunDispatch::new(activation());
        assert!(req.traceparent.is_none());
        let json = serde_json::to_string(&req).expect("serializes");
        assert!(
            !json.contains("traceparent"),
            "a None traceparent is not written: {json}"
        );
        // The same row (no traceparent key) is exactly what a pre-field writer
        // produced; it must load as None.
        let back: RunDispatch = serde_json::from_str(&json).expect("legacy row loads");
        assert!(back.traceparent.is_none());
        assert!(back.execution_scope.is_none());
        assert!(back.session_resources.is_none());
        assert!(back.session_runtime.is_none());
        assert!(back.agent_publications.is_empty());
        assert!(back.placement.is_legacy_default());
    }

    #[test]
    fn non_default_delegation_limits_round_trip_on_the_queue_wire() {
        let mut request = RunDispatch::new(activation());
        request.activation.snapshot.resolved_spec.delegation_limits =
            awaken_agent_contract::agent::delegation::DelegationLimits::new(3, 4, 5);

        let wire = serde_json::to_value(&request).expect("serializes");
        assert_eq!(
            wire["activation"]["snapshot"]["resolved_spec"]["delegation_limits"],
            serde_json::json!({
                "max_depth": 3,
                "max_parallel": 4,
                "max_total": 5
            })
        );
        let restored: RunDispatch = serde_json::from_value(wire).expect("deserializes");
        assert_eq!(
            restored.activation.snapshot.resolved_spec.delegation_limits,
            awaken_agent_contract::agent::delegation::DelegationLimits::new(3, 4, 5)
        );
    }

    #[test]
    fn execution_envelope_round_trips_without_exposing_provider_credentials() {
        // Cause/effect graph: C1 the primary carries a Control-plane reference;
        // C2 the fallback carries a Worker reference and recipient-bound sealed
        // envelope; C3 actual plaintext exists only as CredentialMaterial beside
        // the dispatch; C4 the dispatch is serialized and restored. Effects: E1
        // the two exact references and envelope coordinates persist; E2 only the
        // closed CredentialAccess fields appear; E3 no C3 bytes enter the wire;
        // E4 the complete secret-free request round-trips. Decision table:
        // R1=C1+C4=>E1+E2; R2=C2+C4=>E1+E2; R3=C1+C2+C3+C4=>E3+E4.
        // Constraint: this claim covers credential plaintext represented by
        // RedactedString/CredentialMaterial, not arbitrary user message text.
        let authority = awaken_tenancy::Authority::bound(awaken_agent_contract::ScopeId(
            "workspace-a".to_string(),
        ));
        let claimed = ExecutionScopeRef(awaken_agent_contract::ScopeId("workspace-a".to_string()));
        let verified = authority
            .verify_execution_scope(&claimed)
            .expect("scope belongs to authority");
        let material_plaintext = "credential-plaintext-must-stay-outside-dispatch";
        let _material =
            CredentialMaterial::secret(RedactedString::new(material_plaintext.to_string()));
        let mut activation = activation();
        activation.snapshot.resolved_spec.model_binding =
            provider_candidate("model", "grant-17", "provider@1", "route@1");
        activation.snapshot.resolved_spec.model_candidates = vec![provider_candidate_with_access(
            "fallback-model",
            "worker-grant-23",
            "fallback-provider@2",
            "fallback-route@3",
            CredentialMaterialSource::WorkerReference,
            Some(CredentialEnvelope::SealedForWorker {
                envelope_ref: SealedCredentialEnvelopeRef {
                    id: "sealed-envelope-23".into(),
                    payload_fingerprint: "sha256:sealed-payload-23".into(),
                },
                recipient: TrustDomainRef(
                    awaken_agent_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN.into(),
                ),
                expires_at_unix_ms: 9_999,
            }),
        )];
        let request = RunDispatch::new(activation).with_execution_scope(verified.into_ref());
        let wire = serde_json::to_value(&request).expect("serializes");
        assert_eq!(wire["execution_scope"], "workspace-a");
        let primary = &wire["activation"]["snapshot"]["resolved_spec"]["model_binding"]["provisioning"]
            ["credential"];
        assert_eq!(primary["credential"]["id"], "grant-17", "R1/E1");
        assert_eq!(
            primary["material_source"], "control_plane_reference",
            "R1/E1"
        );
        assert_eq!(
            primary
                .as_object()
                .expect("R1 credential object")
                .keys()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>(),
            std::collections::BTreeSet::from([
                "credential",
                "material_source",
                "policy",
                "target",
                "usage",
            ]),
            "R1/E2"
        );
        let fallback = &wire["activation"]["snapshot"]["resolved_spec"]["model_candidates"][0]["provisioning"]
            ["credential"];
        assert_eq!(fallback["credential"]["id"], "worker-grant-23", "R2/E1");
        assert_eq!(fallback["material_source"], "worker_reference", "R2/E1");
        assert_eq!(
            fallback["envelope"]["envelope_ref"]["id"], "sealed-envelope-23",
            "R2/E1"
        );
        assert_eq!(
            fallback
                .as_object()
                .expect("R2 credential object")
                .keys()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>(),
            std::collections::BTreeSet::from([
                "credential",
                "envelope",
                "material_source",
                "policy",
                "target",
                "usage",
            ]),
            "R2/E2"
        );
        assert!(!wire.to_string().contains(material_plaintext), "R3/E3");
        let restored: RunDispatch = serde_json::from_value(wire).expect("deserializes");
        assert_eq!(restored, request, "R3/E4");
    }

    #[test]
    fn candidate_pool_is_ordered_pinned_and_model_scoped() {
        let mut activation = activation();
        activation.snapshot.resolved_spec.model_binding =
            provider_candidate("primary", "cred-a", "provider-a@1", "route-a@2");
        activation.snapshot.resolved_spec.model_candidates = vec![provider_candidate(
            "fallback",
            "cred-b",
            "provider-b@4",
            "route-b@3",
        )];
        let spec = &activation.snapshot.resolved_spec;
        assert_eq!(
            std::iter::once(&spec.model_binding)
                .chain(spec.model_candidates.iter())
                .map(|candidate| candidate.binding().identity().1)
                .collect::<Vec<_>>(),
            vec!["primary", "fallback"]
        );
        let ModelProvisioning::Provider {
            provider_ref,
            credential: Some(credential),
            ..
        } = spec.model_candidates[0].provisioning()
        else {
            panic!("fallback is provider-backed")
        };
        assert_eq!(credential.credential.id, "cred-b");
        assert_eq!(provider_ref, "provider-b@4");
        assert!(spec.candidate_for_model("not-authored").is_none());
        let wire = serde_json::to_string(&activation).unwrap();
        assert!(!wire.contains("secret"));
        assert_eq!(
            serde_json::from_str::<RunActivation>(&wire).unwrap(),
            activation
        );
    }

    #[test]
    fn host_executor_candidate_is_exact_and_non_secret() {
        let candidate = awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
            ModelBinding::new("host", "embedded-model", "native"),
        );
        assert_eq!(candidate.binding().identity().1, "embedded-model");
        assert!(matches!(
            candidate.provisioning(),
            ModelProvisioning::HostExecutor
        ));
        assert!(
            !serde_json::to_string(&candidate)
                .unwrap()
                .contains("secret")
        );
    }

    #[test]
    fn upgrade_window_identity_normalizes_only_a_retry_against_a_pre_requirement_row() {
        // Cause/effect table: C1 the stored row lacks or carries published MCP
        // requirements; C2 the retry lacks or carries them; C3 the rest of the
        // retry is the same command or not.
        // | Rule | stored | retry | rest | live replay | tombstone replay |
        // | U1 | legacy | current | same | yes | yes |
        // | U2 | current | current | same | yes | yes |
        // | U3 | legacy | legacy | same | yes | yes |
        // | U4 | legacy | current | different | no | no |
        // | U5 | current | legacy | same | no | no |
        // The stored value is never changed by a comparison.
        let envelope =
            |id: &str| SessionRuntimeEnvelope::new(format!("{{\"projection\":\"{id}\"}}"));
        let row = |id: &str, published: bool| {
            let mut request = RunDispatch::new(activation()).with_session_runtime(envelope(id));
            if published {
                request.placement.session_mcp_credentials = Some(Vec::new());
            }
            request
        };
        let tombstone_replays = |stored: &RunDispatch, retry: &RunDispatch| {
            let tombstone = stored.canonical_fingerprint();
            retry.canonical_fingerprint() == tombstone
                || retry.pre_mcp_requirement_canonical_fingerprint().as_deref()
                    == Some(tombstone.as_str())
        };
        let cases = [
            ("U1", row("a", false), row("a", true), true),
            ("U2", row("a", true), row("a", true), true),
            ("U3", row("a", false), row("a", false), true),
            ("U4", row("a", false), row("b", true), false),
            ("U5", row("a", true), row("a", false), false),
        ];
        for (rule, stored, retry, replays) in cases {
            let before = stored.clone();
            assert_eq!(
                stored.same_canonical_dispatch(&retry),
                replays,
                "{rule} live"
            );
            assert_eq!(
                tombstone_replays(&stored, &retry),
                replays,
                "{rule} tombstone"
            );
            assert_eq!(stored, before, "{rule} the stored row is unchanged");
        }
        assert_eq!(
            row("a", false).pre_mcp_requirement_canonical_fingerprint(),
            None,
            "a retry without requirements has no pre-requirement form"
        );
    }

    #[test]
    fn strict_worker_requirements_are_pinned_while_legacy_bytes_stay_unchanged() {
        let request =
            RunDispatch::new(activation()).with_placement(PlacementRequirements::remote_required());
        let wire = serde_json::to_value(&request).expect("serializes");
        assert_eq!(wire["placement"]["contract_version"], 1);
        assert_eq!(wire["placement"]["location"], "remote_required");
        let restored: RunDispatch = serde_json::from_value(wire).expect("deserializes");
        assert_eq!(restored, request);
    }

    /// Resource-envelope ACL cause/effect decision table. Causes: C1 a valid
    /// Session manifest; C2 an envelope with valid resource JSON; C3 malformed
    /// resource JSON. Effects: E1 workspace/revision and resources are encoded
    /// losslessly; E2 the exact manifest is restored; E3 decoding fails closed.
    /// Rules A1 C1=>E1, A2 C1+C2=>E2, A3 C3=>E3 cover every JSON validity class.
    #[test]
    fn session_resource_envelope_is_the_single_lossless_fail_closed_acl() {
        let manifest = SessionResourceManifest::at_revision("workspace-a", 7, Default::default());
        let envelope = SessionResourceEnvelope::at_revision(
            manifest.workspace_id.clone(),
            manifest.revision,
            manifest.encode_resources().expect("A1 encode"),
        );
        assert_eq!(envelope.workspace_id, "workspace-a", "A1");
        assert_eq!(envelope.resource_revision, 7, "A1");
        assert_eq!(
            envelope
                .decode_with(SessionResourceManifest::decode_resources)
                .expect("A2 decode"),
            manifest
        );

        let malformed = SessionResourceEnvelope::at_revision("workspace-a", 7, "{");
        assert!(
            malformed
                .decode_with(SessionResourceManifest::decode_resources)
                .is_err(),
            "A3"
        );
    }
}
