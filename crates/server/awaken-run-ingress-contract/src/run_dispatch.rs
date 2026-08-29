//! One Run accepted into durable dispatch.
//!
//! [`RunDispatch`] is exactly the data a durable queue persists and replays. It
//! carries no live handles (G3/G4), so any worker can rebuild the Run after a
//! crash. Live worker dependencies stay in the `awaken-run-ingress` host.

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_provisioning_contract::SandboxCapacityShapeId;
use awaken_runtime_contract::PlaintextHolder;
use awaken_runtime_contract::activation::RunActivation;
pub use awaken_tenancy::ExecutionScopeRef;
pub use awaken_worker_contract::PlacementRequirements;
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

    /// Encode the Session-owned manifest into its one durable dispatch shape.
    pub fn from_manifest(
        manifest: &awaken_session_contract::SessionResourceManifest,
    ) -> Result<Self, serde_json::Error> {
        Ok(Self::at_revision(
            manifest.workspace_id.clone(),
            manifest.revision,
            serde_json::to_string(&manifest.resources)?,
        ))
    }

    /// Restore the Session-owned manifest before validation or realization.
    pub fn decode_manifest(
        &self,
    ) -> Result<awaken_session_contract::SessionResourceManifest, serde_json::Error> {
        Ok(
            awaken_session_contract::SessionResourceManifest::at_revision(
                self.workspace_id.clone(),
                self.resource_revision,
                serde_json::from_str(&self.resolved_resources_json)?,
            ),
        )
    }
}

/// Worker-side action for one frozen Session Resource generation. The decision
/// is kept in the durable-ingress contract so every Runtime adapter applies the
/// same replay, revision, and Workspace fence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionResourceInstallDecision {
    Stage,
    Replace,
    Reject,
}

/// Decide whether an incoming durable generation is a first/exact staging, an
/// authorized replacement, or a stale/cross-Workspace rejection. Only the
/// Session authority may amend an unattempted dispatch projection in place;
/// claimed Workers remain fenced from changing same-revision content.
#[must_use]
pub const fn session_resource_install_decision(
    previous_exists: bool,
    exact_replay: bool,
    workspace_matches: bool,
    previous_revision: u64,
    incoming_revision: u64,
    authority_amends_unattempted: bool,
) -> SessionResourceInstallDecision {
    if !previous_exists || exact_replay {
        return SessionResourceInstallDecision::Stage;
    }
    if workspace_matches
        && (incoming_revision > previous_revision
            || (authority_amends_unattempted && incoming_revision == previous_revision))
    {
        SessionResourceInstallDecision::Replace
    } else {
        SessionResourceInstallDecision::Reject
    }
}

/// Canonical secret-free payload carried by [`SessionRuntimeEnvelope`].
///
/// The durable ingress contract owns this wire shape because both claim
/// admission and Runtime realization must inspect the same frozen facts. The
/// Session aggregate remains the desired-state authority; this is only its
/// dispatch projection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DispatchedSessionRuntimeProjection {
    pub environment: awaken_session_contract::EnvironmentSnapshot,
    /// `None` retains publication inheritance; `Some(default())` explicitly
    /// clears the complete Session tool configuration.
    #[serde(default)]
    pub tools: Option<awaken_session_contract::SessionToolConfiguration>,
    /// `None` is reserved for legacy rows predating MCP effect projection. New
    /// dispatches always carry `Some`, including `Some([])`.
    #[serde(default)]
    pub mcp_stages: Option<Vec<awaken_session_contract::StageMcpAttachment>>,
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

    pub fn from_projection(
        environment: awaken_session_contract::EnvironmentSnapshot,
        tools: Option<awaken_session_contract::SessionToolConfiguration>,
        mcp_stages: Vec<awaken_session_contract::StageMcpAttachment>,
    ) -> Result<Self, serde_json::Error> {
        Ok(Self::new(serde_json::to_string(
            &DispatchedSessionRuntimeProjection {
                environment,
                tools,
                mcp_stages: Some(mcp_stages),
            },
        )?))
    }

    pub fn decode_projection(
        &self,
    ) -> Result<DispatchedSessionRuntimeProjection, serde_json::Error> {
        serde_json::from_str(&self.projection_json)
    }
}

#[cfg(kani)]
mod kani_proofs {
    use super::{SessionResourceInstallDecision, session_resource_install_decision};

    #[kani::proof]
    fn session_resource_replacement_requires_newer_or_authority_amended_generation() {
        let previous_exists: bool = kani::any();
        let exact_replay: bool = kani::any();
        let workspace_matches: bool = kani::any();
        let previous_revision: u64 = kani::any();
        let incoming_revision: u64 = kani::any();
        let authority_amends_unattempted: bool = kani::any();

        let decision = session_resource_install_decision(
            previous_exists,
            exact_replay,
            workspace_matches,
            previous_revision,
            incoming_revision,
            authority_amends_unattempted,
        );
        assert_eq!(
            decision == SessionResourceInstallDecision::Replace,
            previous_exists
                && !exact_replay
                && workspace_matches
                && (incoming_revision > previous_revision
                    || (authority_amends_unattempted && incoming_revision == previous_revision))
        );
        assert_eq!(
            decision == SessionResourceInstallDecision::Reject,
            previous_exists
                && !exact_replay
                && (!workspace_matches
                    || incoming_revision < previous_revision
                    || (incoming_revision == previous_revision && !authority_amends_unattempted))
        );
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
    pub session_command_fingerprint: Option<awaken_session_contract::SessionRunCommandFingerprint>,
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
    pub session_run_replacement: awaken_session_contract::SessionRunReplacement,
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

impl RunDispatch {
    /// Classify the immutable routing/activity coordinates before persistence.
    /// The caller still owns port-specific authorization; this method only makes
    /// the previously implicit shape distinction total and shared.
    #[must_use]
    pub fn admission_shape(&self) -> DispatchAdmissionShape {
        if self.session_activity_epoch == Some(0) {
            return DispatchAdmissionShape::InvalidZeroActivityEpoch;
        }
        match self.session_thread_id.as_ref() {
            None => DispatchAdmissionShape::OrdinaryRoot,
            Some(session) if session == self.thread_id() => {
                if self.session_activity_epoch.is_some() {
                    DispatchAdmissionShape::SessionRootWithActivity
                } else {
                    DispatchAdmissionShape::SessionRootAwaitingActivity
                }
            }
            Some(_) => DispatchAdmissionShape::SessionChild,
        }
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
            &self.activation.tool_capability_narrowing,
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
            session_run_replacement: awaken_session_contract::SessionRunReplacement::PreservePrior,
            traceparent: None,
            execution_scope: None,
            session_resources: None,
            session_runtime: None,
            agent_publications: Vec::new(),
            preferred_environment_shape: None,
            inference_plaintext_holder: None,
            placement: PlacementRequirements::default(),
        }
    }

    /// Stable dispatch identity for caller-owned Run ids. Distributed tracing is
    /// intentionally excluded: a retry may arrive under another request span,
    /// while every execution-bearing field must remain byte-equivalent.
    #[must_use]
    pub fn canonical_fingerprint(&self) -> String {
        let fingerprint = awaken_runtime_contract::content_fingerprint(&self.canonicalized())
            .expect("RunDispatch's serializable contract has no fallible value");
        format!("sha256:{fingerprint}")
    }

    /// Canonical-byte replay comparison for a live row. Hashes are needed only
    /// once the full dispatch has been compacted into a completion tombstone.
    #[must_use]
    pub fn same_canonical_dispatch(&self, other: &Self) -> bool {
        self.canonical_bytes() == other.canonical_bytes()
    }

    /// Compare only the immutable Session command identity. This method is for
    /// the dedicated root-reservation port; ordinary and child Run admissions
    /// continue to use [`Self::same_canonical_dispatch`].
    #[must_use]
    pub fn same_session_reservation(&self, other: &Self) -> bool {
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
            || self.legacy_session_reservation_fingerprint(),
            |fingerprint| fingerprint.as_str().to_string(),
        )
    }

    /// Pre-versioned compact identity retained only for comparing a current
    /// incoming retry with a completion or live row written by an older build.
    /// Callers must choose it from stored legacy evidence, never from an
    /// incoming request.
    #[must_use]
    pub fn legacy_session_reservation_fingerprint(&self) -> String {
        let fingerprint = awaken_runtime_contract::content_fingerprint(&(
            &self.activation.run_id,
            &self.activation.thread_id,
            &self.activation.input,
            &self.activation.delegation_origin,
            &self.activation.data_subject_id,
            &self.activation.tool_capability_narrowing,
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
        match self.identity_scope {
            DispatchIdentityScope::SessionCommand
                if matches!(
                    other.admission_shape(),
                    DispatchAdmissionShape::SessionRootAwaitingActivity
                        | DispatchAdmissionShape::SessionRootWithActivity
                ) =>
            {
                self.same_session_reservation(other)
            }
            DispatchIdentityScope::SessionCommand | DispatchIdentityScope::FullDispatch => {
                self.same_canonical_dispatch(other)
            }
        }
    }

    /// Tombstone twin of [`Self::same_admission_dispatch`]. Live and completed
    /// replay can therefore never disagree about the accepted Run identity.
    #[must_use]
    pub fn admission_fingerprint(&self) -> String {
        match self.identity_scope {
            DispatchIdentityScope::SessionCommand => self.session_reservation_fingerprint(),
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

    /// Attach the Session authority's command identity. The reservation port
    /// separately marks the accepted row's identity scope after validation.
    #[must_use]
    pub fn with_session_command_fingerprint(
        mut self,
        fingerprint: awaken_session_contract::SessionRunCommandFingerprint,
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
    pub fn with_session_run_replacement(
        mut self,
        replacement: awaken_session_contract::SessionRunReplacement,
    ) -> Self {
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn session_resource_install_decision_follows_the_complete_table() {
        use SessionResourceInstallDecision::{Reject, Replace, Stage};

        // Cause/effect graph: C1 previous manifest exists; C2 exact replay;
        // C3 Workspace matches; C4 incoming revision is newer/equal/older; C5
        // the Session authority proves an unattempted in-place amendment.
        // Effects are Stage, Replace, or Reject. Claimed Workers always have
        // !C5, so same-revision content replacement remains impossible.
        //
        // | Rule | C1 | C2 | C3 | C4 | C5 | Effect |
        // | I1 | F | any | any | any | any | Stage |
        // | I2 | T | T | any | any | any | Stage |
        // | I3 | T | F | T | newer | any | Replace |
        // | I4 | T | F | T | equal | T | Replace |
        // | I5 | T | F | T | equal | F | Reject |
        // | I6 | T | F | T | older | any | Reject |
        // | I7 | T | F | F | any | any | Reject |
        let rules = [
            (false, false, false, 9, 0, false, Stage),
            (true, true, false, 9, 0, false, Stage),
            (true, false, true, 9, 10, false, Replace),
            (true, false, true, 9, 9, true, Replace),
            (true, false, true, 9, 9, false, Reject),
            (true, false, true, 9, 8, true, Reject),
            (true, false, false, 9, 10, true, Reject),
        ];
        for (previous, replay, workspace, old, incoming, amendment, expected) in rules {
            assert_eq!(
                session_resource_install_decision(
                    previous,
                    previous && replay,
                    workspace,
                    old,
                    incoming,
                    amendment,
                ),
                expected
            );
        }
    }
    use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
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
        awaken_runtime_contract::resolved::ResolvedModelCandidate::try_provider(
            ModelBinding::new(provider, model, "genai"),
            provider,
            route,
            "workspace-a",
            Some(
                awaken_runtime_contract::CredentialAccess::new(
                    awaken_runtime_contract::CredentialRef {
                        id: credential.into(),
                        revision: 0,
                    },
                    awaken_runtime_contract::CredentialMaterialSource::ControlPlaneReference,
                    awaken_runtime_contract::CredentialUsage::ProviderAdapter,
                    awaken_runtime_contract::CredentialExecutionPolicy::self_hosted_provider(),
                )
                .with_target(awaken_runtime_contract::CredentialTarget::new(
                    awaken_runtime_contract::credential::CredentialPurpose::ProviderAdapter,
                    provider.split_once('@').map_or(provider, |(id, _)| id),
                )),
            ),
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
                    max_steps: 8,
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

    fn command_fingerprint(fill: char) -> awaken_session_contract::SessionRunCommandFingerprint {
        serde_json::from_value(serde_json::Value::String(format!(
            "{}{}",
            awaken_session_contract::SessionRunCommandFingerprint::CURRENT_PREFIX,
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
        reprojected.execution_scope = Some(ExecutionScopeRef(awaken_tenancy::ScopeId::from(
            "current-workspace",
        )));
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
                .starts_with(awaken_session_contract::SessionRunCommandFingerprint::CURRENT_PREFIX),
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
    }

    /// Resource-envelope compatibility causes/effects: C1 current dispatch has a
    /// Session generation; C2 legacy dispatch omits it. E1 C1 round-trips the
    /// exact generation; E2 C2 decodes as generation zero without dead-lettering.
    #[test]
    fn frozen_session_resources_round_trip_with_their_matching_scope() {
        let resources =
            SessionResourceEnvelope::at_revision("workspace-a", 4, r#"{"inputs":[],"skills":[]}"#);
        let request = RunDispatch::new(activation())
            .with_execution_scope(ExecutionScopeRef(awaken_tenancy::ScopeId::from(
                "workspace-a",
            )))
            .with_session_resources(resources.clone());

        let json = serde_json::to_string(&request).expect("serializes");
        let recovered: RunDispatch = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(recovered.session_resources, Some(resources));
        assert_eq!(
            recovered.execution_scope,
            Some(ExecutionScopeRef(awaken_tenancy::ScopeId::from(
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
        let environment = awaken_session_contract::EnvironmentSnapshot {
            environment_id: "environment-a".into(),
            revision: awaken_session_contract::EnvironmentRevision(1),
            self_hosted: true,
            config_fingerprint: awaken_session_contract::EnvironmentFingerprint(
                "environment-a@1".into(),
            ),
            sandbox: Default::default(),
            sandbox_provisioning: Default::default(),
            idle_retention: Default::default(),
            packages: Default::default(),
            prepared_image: None,
            network: awaken_session_contract::SessionNetworkPolicy::Unrestricted,
            credential_realization:
                awaken_runtime_contract::CredentialRealizationProfile::self_hosted_native(),
        };
        let tools = awaken_session_contract::SessionToolConfiguration {
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
        let runtime = SessionRuntimeEnvelope::from_projection(
            environment.clone(),
            Some(tools.clone()),
            Vec::new(),
        )
        .expect("P1 encode");
        let projection = runtime.decode_projection().expect("P1 decode");
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
        let authority =
            awaken_tenancy::Authority::bound(awaken_tenancy::ScopeId("workspace-a".to_string()));
        let claimed = ExecutionScopeRef(awaken_tenancy::ScopeId("workspace-a".to_string()));
        let verified = authority
            .verify_execution_scope(&claimed)
            .expect("scope belongs to authority");
        let mut activation = activation();
        activation.snapshot.resolved_spec.model_binding =
            provider_candidate("model", "grant-17", "provider@1", "route@1");
        let request = RunDispatch::new(activation).with_execution_scope(verified.into_ref());
        let wire = serde_json::to_value(&request).expect("serializes");
        assert_eq!(wire["execution_scope"], "workspace-a");
        assert_eq!(
            wire["activation"]["snapshot"]["resolved_spec"]["model_binding"]["provisioning"]["credential"]
                ["credential"]["id"],
            "grant-17"
        );
        assert!(!wire.to_string().contains("provider-key"));
        let restored: RunDispatch = serde_json::from_value(wire).expect("deserializes");
        assert_eq!(restored, request);
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
                .map(|candidate| candidate.binding().model_ref.as_str())
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
        assert_eq!(candidate.binding().model_ref, "embedded-model");
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
        let manifest = awaken_session_contract::SessionResourceManifest::at_revision(
            "workspace-a",
            7,
            Default::default(),
        );
        let envelope = SessionResourceEnvelope::from_manifest(&manifest).expect("A1 encode");
        assert_eq!(envelope.workspace_id, "workspace-a", "A1");
        assert_eq!(envelope.resource_revision, 7, "A1");
        assert_eq!(envelope.decode_manifest().expect("A2 decode"), manifest);

        let malformed = SessionResourceEnvelope::at_revision("workspace-a", 7, "{");
        assert!(malformed.decode_manifest().is_err(), "A3");
    }
}
