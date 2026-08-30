//! One crash-safe filesystem-realization authority shared by Local and
//! Namespace sandbox adapters.
//!
//! Session owns authorization. This provider sidecar persists only the exact
//! projected effect fence, immutable spec, physical incarnation, root inode,
//! and lifecycle crash cut needed to apply that authorization safely.

use std::path::{Component, Path, PathBuf};

use awaken_provisioning_contract as pc;
use awaken_sandbox_fs::{DirectoryIdentity, PathEntry};
use serde::{Deserialize, Serialize};

const MARKER_SCHEMA: u8 = 2;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RealizationMarker {
    schema: u8,
    fingerprint: pc::SandboxRealizationFingerprint,
    effect_fence: pc::SandboxEffectFence,
    physical_incarnation: String,
    phase: RealizationPhase,
    root: RootRecord,
    #[serde(skip_serializing_if = "Option::is_none")]
    rebuild_source: Option<RebuildSourceRecord>,
    /// The sole provider participant for a checkpoint upload. Defaulting keeps
    /// pre-participant schema-2 markers readable without a parallel marker
    /// version or lifecycle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    checkpoint: Option<CheckpointParticipant>,
    /// Immutable evidence consumed when the same lifecycle crosses into
    /// terminal cleanup. The marker's top-level fence is the last provider
    /// preparation admitted by this lifecycle; effect-free reconstruction may
    /// validate a successor but only the retained `RemovalGuard` may advance
    /// it. The physical realization fence and an optional in-flight operation
    /// fence remain available for response-loss replay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    terminal_source: Option<TerminalSourceRecord>,
    /// The aggregate-derived two-stage physical deletion authority. The value
    /// itself is the canonical codec: its prepared fence and fingerprint remain
    /// immutable while only its latest authorized successor may advance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    disposal_authorization: Option<pc::SandboxDisposalAuthorization>,
    /// Canonical immutable checkpoint input for a fenced restore. Keeping this
    /// in the realization marker makes same-effect response-loss replay exact;
    /// a legacy marker with no evidence cannot be upgraded by overwriting Ready.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    restore_input_fingerprint: Option<String>,
    /// Exact provider result published atomically with `Ready`. A retry reads
    /// this receipt instead of repeating mount, Memory materialization, or
    /// checkpoint extraction effects against an already-usable root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    completion: Option<RealizationCompletionReceipt>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RealizationPhase {
    Creating,
    Recreating,
    Ready,
    Removing,
    Removed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RootRecord {
    private_stage_leaf: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    identity: Option<SerializedDirectoryIdentity>,
    published: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SerializedDirectoryIdentity {
    device: u64,
    inode: u64,
}

impl From<DirectoryIdentity> for SerializedDirectoryIdentity {
    fn from(value: DirectoryIdentity) -> Self {
        Self {
            device: value.device,
            inode: value.inode,
        }
    }
}

impl From<SerializedDirectoryIdentity> for DirectoryIdentity {
    fn from(value: SerializedDirectoryIdentity) -> Self {
        Self {
            device: value.device,
            inode: value.inode,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RebuildSourceRecord {
    #[serde(default = "ready_phase")]
    phase: RealizationPhase,
    effect_fence: pc::SandboxEffectFence,
    physical_incarnation: String,
    root: RootRecord,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    checkpoint: Option<CheckpointParticipant>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    terminal_source: Option<TerminalSourceRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    disposal_authorization: Option<pc::SandboxDisposalAuthorization>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    restore_input_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    completion: Option<RealizationCompletionReceipt>,
}

fn ready_phase() -> RealizationPhase {
    RealizationPhase::Ready
}

mod receipt;
pub(crate) use receipt::RealizationCompletionReceipt;
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointParticipant {
    /// Exact checkpoint operation recorded when the participant WAL is first
    /// published. It remains the Suspend identity after the marker's top-level
    /// fence advances to terminal cleanup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    operation_id: Option<String>,
    request_fingerprint: String,
    snapshot_digest: String,
    snapshot_size_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reference: Option<pc::SandboxCheckpointRef>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TerminalSourceRecord {
    realization_effect_fence: pc::SandboxEffectFence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expected_effect_fence: Option<pc::SandboxEffectFence>,
}

/// Exact V2 handle evidence required to authorize a same-path rebuild.
pub(crate) struct RebuildSource<'a> {
    pub(crate) fingerprint: &'a pc::SandboxRealizationFingerprint,
    pub(crate) effect_fence: &'a pc::SandboxEffectFence,
    pub(crate) physical_incarnation: &'a str,
}

pub(crate) fn rebuild_source(
    handle: &pc::SandboxHandle,
) -> Result<RebuildSource<'_>, pc::SandboxError> {
    Ok(RebuildSource {
        fingerprint: handle
            .realization_fingerprint()
            .ok_or_else(|| err("legacy sandbox handle cannot authorize rebuild"))?,
        effect_fence: handle
            .filesystem_effect_fence()?
            .ok_or_else(|| err("legacy sandbox handle has no rebuild effect fence"))?,
        physical_incarnation: handle
            .filesystem_physical_incarnation()?
            .ok_or_else(|| err("legacy sandbox handle has no physical rebuild incarnation"))?,
    })
}

/// Provider evidence carried by a live current sandbox and projected into its
/// V2 durable handle. Root inode evidence remains provider-private.
#[derive(Clone, Debug)]
pub(crate) struct RealizationEvidence {
    fingerprint: pc::SandboxRealizationFingerprint,
    effect_fence: pc::SandboxEffectFence,
    physical_incarnation: String,
    root_identity: DirectoryIdentity,
}

impl RealizationEvidence {
    pub(crate) fn fingerprint(&self) -> &pc::SandboxRealizationFingerprint {
        &self.fingerprint
    }

    pub(crate) fn effect_fence(&self) -> &pc::SandboxEffectFence {
        &self.effect_fence
    }

    pub(crate) fn physical_incarnation(&self) -> &str {
        &self.physical_incarnation
    }

    pub(crate) fn root_identity(&self) -> DirectoryIdentity {
        self.root_identity
    }
}

/// A deliberately unfenced, one-process filesystem root. It may be emitted as
/// V1, but only this live object has deletion evidence; adoption never gains it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LegacyLiveEvidence {
    root_identity: DirectoryIdentity,
}

impl LegacyLiveEvidence {
    pub(crate) fn root_identity(self) -> DirectoryIdentity {
        self.root_identity
    }
}

/// Complete filesystem-realization authority carried by a live concrete
/// sandbox. This one enum prevents fingerprint, fence, incarnation, and
/// destructive-root evidence from drifting across parallel optional fields.
#[derive(Clone, Debug)]
pub(crate) enum LiveRealization {
    LegacyCreated(LegacyLiveEvidence),
    LegacyAdopted,
    Current(RealizationEvidence),
}

impl LiveRealization {
    pub(crate) fn current(&self) -> Option<&RealizationEvidence> {
        match self {
            Self::Current(evidence) => Some(evidence),
            Self::LegacyCreated(_) | Self::LegacyAdopted => None,
        }
    }

    pub(crate) fn legacy_live_identity(&self) -> Option<DirectoryIdentity> {
        match self {
            Self::LegacyCreated(evidence) => Some(evidence.root_identity()),
            Self::LegacyAdopted | Self::Current(_) => None,
        }
    }

    /// Bind a provider operation to the physical root represented by this live
    /// object. Current and locally-created legacy roots require their exact
    /// captured inode; decode-only V1 adoption may observe an existing directory
    /// for non-destructive compatibility but never gains deletion authority.
    pub(crate) fn root_identity_for_access(
        &self,
        root: &Path,
    ) -> Result<Option<DirectoryIdentity>, pc::SandboxError> {
        let entry = classify(root)?;
        match (self, entry) {
            (_, PathEntry::Absent) => Ok(None),
            (Self::Current(evidence), PathEntry::Directory(observed))
                if observed == evidence.root_identity() =>
            {
                Ok(Some(observed))
            }
            (Self::LegacyCreated(evidence), PathEntry::Directory(observed))
                if observed == evidence.root_identity() =>
            {
                Ok(Some(observed))
            }
            (Self::LegacyAdopted, PathEntry::Directory(observed)) => Ok(Some(observed)),
            _ => Err(err(format!(
                "sandbox root `{}` has a foreign file type or physical identity",
                root.display()
            ))),
        }
    }

    pub(crate) fn require_root_identity(
        &self,
        root: &Path,
    ) -> Result<DirectoryIdentity, pc::SandboxError> {
        self.root_identity_for_access(root)?
            .ok_or_else(|| err(format!("sandbox root `{}` is absent", root.display())))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Admission {
    Creating,
    Recreating,
    Ready,
}

/// The single-writer guard for one fenced realization transition.
pub(crate) struct CreationGuard {
    root: PathBuf,
    marker: RealizationMarker,
    admission: Admission,
    lock: Option<awaken_sandbox_fs::ExclusiveFileLock>,
}

impl CreationGuard {
    fn lock(&self) -> Result<&awaken_sandbox_fs::ExclusiveFileLock, pc::SandboxError> {
        self.lock
            .as_ref()
            .ok_or_else(|| err("filesystem creation lifecycle lock was released"))
    }

    #[cfg(test)]
    pub(crate) fn admission(&self) -> Admission {
        self.admission
    }

    pub(crate) fn completed_receipt(
        &self,
    ) -> Result<Option<&RealizationCompletionReceipt>, pc::SandboxError> {
        if self.admission != Admission::Ready {
            return Ok(None);
        }
        self.marker
            .completion
            .as_ref()
            .ok_or_else(|| err("Ready filesystem realization has no exact completion receipt"))
            .map(Some)
    }

    pub(crate) fn validate_before_mutation(&self) -> Result<(), pc::SandboxError> {
        validate_live_effect_fence(&self.marker.effect_fence)?;
        let observed = read_marker_locked(&self.root, self.lock()?)?
            .ok_or_else(|| err("filesystem creation lost its realization marker"))?;
        if observed != self.marker {
            return Err(err(
                "filesystem creation marker changed under its held lifecycle lock",
            ));
        }
        require_exact_root_locked(
            &self.root,
            self.lock()?,
            required_root_identity(&self.marker)?,
        )
    }

    /// Clear only descendants of the exact root inode owned by an incomplete
    /// attempt. Keeping the root inode stable prevents same-effect ABA.
    pub(crate) fn prepare_root(&self) -> Result<(), pc::SandboxError> {
        match self.admission {
            Admission::Creating | Admission::Recreating => {
                validate_live_effect_fence(&self.marker.effect_fence)?;
                let identity = required_root_identity(&self.marker)?;
                require_exact_root_locked(&self.root, self.lock()?, identity)?;
                self.lock()?
                    .clear_sibling_directory_contents_exact(root_leaf(&self.root)?, identity)
                    .map_err(err)
            }
            Admission::Ready => Ok(()),
        }
    }

    /// Publish Ready only after all provider materialization succeeds.
    pub(crate) fn complete(
        &mut self,
        receipt: &RealizationCompletionReceipt,
    ) -> Result<RealizationEvidence, pc::SandboxError> {
        receipt.validate()?;
        let identity = required_root_identity(&self.marker)?;
        require_exact_root_locked(&self.root, self.lock()?, identity)?;
        if self.admission == Admission::Ready {
            let observed = read_marker_locked(&self.root, self.lock()?)?
                .ok_or_else(|| err("Ready filesystem replay lost its realization marker"))?;
            if observed != self.marker || self.marker.completion.as_ref() != Some(receipt) {
                return Err(err(
                    "Ready filesystem response-loss replay changed its completion receipt",
                ));
            }
        } else {
            validate_live_effect_fence(&self.marker.effect_fence)?;
            let mut next = self.marker.clone();
            next.phase = RealizationPhase::Ready;
            next.rebuild_source = None;
            next.completion = Some(receipt.clone());
            replace_marker_locked(&self.root, self.lock()?, &self.marker, &next)?;
            self.marker = next;
            self.admission = Admission::Ready;
        }
        let realization = evidence(&self.marker)?;
        self.lock.take();
        Ok(realization)
    }

    /// Compensate one deterministic failure. A failed cleanup preserves the
    /// marker and exact root evidence so a later retry cannot mistake bytes for
    /// foreign or completed state.
    pub(crate) fn abort_creation(self) -> Result<(), pc::SandboxError> {
        if self.admission == Admission::Ready {
            return Ok(());
        }
        validate_live_effect_fence(&self.marker.effect_fence)?;
        remove_owned_root_or_stage_locked(&self.root, self.lock()?, &self.marker)?;
        match self.marker.phase {
            RealizationPhase::Creating => {
                remove_marker_exact_locked(&self.root, self.lock()?, &self.marker)
            }
            RealizationPhase::Recreating => {
                let source =
                    self.marker.rebuild_source.as_ref().ok_or_else(|| {
                        err("recreating sandbox marker has no exact rebuild source")
                    })?;
                let restored = RealizationMarker {
                    schema: MARKER_SCHEMA,
                    fingerprint: self.marker.fingerprint.clone(),
                    effect_fence: source.effect_fence.clone(),
                    physical_incarnation: source.physical_incarnation.clone(),
                    phase: source.phase,
                    root: source.root.clone(),
                    rebuild_source: None,
                    checkpoint: source.checkpoint.clone(),
                    terminal_source: source.terminal_source.clone(),
                    disposal_authorization: source.disposal_authorization.clone(),
                    restore_input_fingerprint: source.restore_input_fingerprint.clone(),
                    completion: source.completion.clone(),
                };
                replace_marker_locked(&self.root, self.lock()?, &self.marker, &restored)
            }
            _ => Err(err("sandbox creation guard entered a non-creation phase")),
        }
    }
}

/// One-shot guard for explicitly unfenced callers. It owns no durable marker
/// and therefore cannot be replayed or adopted as destructive authority.
pub(crate) struct LegacyCreationGuard {
    root: PathBuf,
    identity: Option<DirectoryIdentity>,
    lock: Option<awaken_sandbox_fs::ExclusiveFileLock>,
}

impl LegacyCreationGuard {
    fn lock(&self) -> Result<&awaken_sandbox_fs::ExclusiveFileLock, pc::SandboxError> {
        self.lock
            .as_ref()
            .ok_or_else(|| err("legacy filesystem creation lock was released"))
    }

    pub(crate) fn validate_before_mutation(&self) -> Result<(), pc::SandboxError> {
        require_marker_absence_locked(&self.root, self.lock()?)?;
        let identity = self
            .identity
            .ok_or_else(|| err("legacy sandbox root was not prepared"))?;
        require_exact_root_locked(&self.root, self.lock()?, identity)
    }

    pub(crate) fn prepare_root(&mut self) -> Result<(), pc::SandboxError> {
        require_marker_absence_locked(&self.root, self.lock()?)?;
        require_absent_locked(
            &self.root,
            self.lock()?,
            root_leaf(&self.root)?,
            "legacy sandbox root",
        )?;
        self.identity = Some(
            self.lock()?
                .create_sibling_directory_noreplace(root_leaf(&self.root)?)
                .map_err(err)?,
        );
        Ok(())
    }

    pub(crate) fn complete(&mut self) -> Result<LegacyLiveEvidence, pc::SandboxError> {
        let identity = self
            .identity
            .ok_or_else(|| err("legacy sandbox root was not prepared"))?;
        require_exact_root_locked(&self.root, self.lock()?, identity)?;
        let realization = LegacyLiveEvidence {
            root_identity: identity,
        };
        self.lock.take();
        Ok(realization)
    }

    pub(crate) fn abort_creation(self) -> Result<(), pc::SandboxError> {
        if let Some(identity) = self.identity {
            self.lock()?
                .remove_sibling_directory_tree_exact(root_leaf(&self.root)?, identity)
                .map_err(err)?;
        }
        Ok(())
    }
}

/// One provider creation guard across fenced current and explicitly legacy
/// callers. It centralizes preparation/compensation without making legacy state
/// a second durable lifecycle.
pub(crate) enum ProviderCreationGuard {
    Current(Box<CreationGuard>),
    Legacy(LegacyCreationGuard),
}

impl ProviderCreationGuard {
    pub(crate) fn is_incomplete(&self) -> bool {
        match self {
            Self::Current(guard) => guard.admission != Admission::Ready,
            Self::Legacy(_) => true,
        }
    }

    pub(crate) fn completed_receipt(
        &self,
    ) -> Result<Option<&RealizationCompletionReceipt>, pc::SandboxError> {
        match self {
            Self::Current(guard) => guard.completed_receipt(),
            Self::Legacy(_) => Ok(None),
        }
    }

    /// Exact root identity already owned by this admission. Current attempts
    /// publish it before returning from `begin`; a legacy attempt remains absent
    /// until `prepare_root` creates its one-shot root.
    pub(crate) fn root_identity(&self) -> Result<Option<DirectoryIdentity>, pc::SandboxError> {
        match self {
            Self::Current(guard) => required_root_identity(&guard.marker).map(Some),
            Self::Legacy(guard) => Ok(guard.identity),
        }
    }

    pub(crate) fn prepare_root(&mut self) -> Result<(), pc::SandboxError> {
        match self {
            Self::Current(guard) => guard.prepare_root(),
            Self::Legacy(guard) => guard.prepare_root(),
        }
    }

    pub(crate) fn validate_before_mutation(&self) -> Result<(), pc::SandboxError> {
        match self {
            Self::Current(guard) => guard.validate_before_mutation(),
            Self::Legacy(guard) => guard.validate_before_mutation(),
        }
    }

    pub(crate) fn complete(
        &mut self,
        receipt: &RealizationCompletionReceipt,
    ) -> Result<LiveRealization, pc::SandboxError> {
        match self {
            Self::Current(guard) => guard.complete(receipt).map(LiveRealization::Current),
            Self::Legacy(guard) => guard.complete().map(LiveRealization::LegacyCreated),
        }
    }

    pub(crate) fn abort_creation(self) -> Result<(), pc::SandboxError> {
        match self {
            Self::Current(guard) => (*guard).abort_creation(),
            Self::Legacy(guard) => guard.abort_creation(),
        }
    }
}

/// Serializes exact terminal cleanup and retains a Removed tombstone for
/// delete/finish/receipt response-loss replay.
pub(crate) struct RemovalGuard {
    root: PathBuf,
    marker: RealizationMarker,
    checkpoint_expected_effect_fence: Option<pc::SandboxEffectFence>,
    lock: awaken_sandbox_fs::ExclusiveFileLock,
}

impl RemovalGuard {
    fn validate_disposal_authorization(&self) -> Result<(), pc::SandboxError> {
        let authorization = self.marker.disposal_authorization.as_ref().ok_or_else(|| {
            err("filesystem physical mutation has no aggregate authorization authority")
        })?;
        validate_live_effect_fence(authorization.effect_fence())
    }

    pub(crate) fn completed_receipt(
        &self,
    ) -> Result<Option<&RealizationCompletionReceipt>, pc::SandboxError> {
        if self.marker.phase == RealizationPhase::Removed {
            return Ok(self.marker.completion.as_ref());
        }
        if self.marker.terminal_source.is_some() && self.marker.completion.is_none() {
            return Err(err(
                "completed filesystem realization entered terminal cleanup without its receipt",
            ));
        }
        Ok(self.marker.completion.as_ref())
    }

    pub(crate) fn refresh_authorization(
        &mut self,
        effect_fence: &pc::SandboxEffectFence,
    ) -> Result<(), pc::SandboxError> {
        if self.marker.disposal_authorization.is_some() {
            return Err(err(
                "filesystem realization already entered aggregate-authorized physical authorization",
            ));
        }
        validate_live_effect_fence(effect_fence)?;
        if !self.marker.effect_fence.authorizes_successor(effect_fence) {
            return Err(err(
                "terminal disposal fence is stale, foreign, or regresses lease expiry",
            ));
        }
        if &self.marker.effect_fence != effect_fence {
            let mut next = self.marker.clone();
            next.effect_fence = effect_fence.clone();
            replace_marker_locked(&self.root, &self.lock, &self.marker, &next)?;
            self.marker = next;
        }
        Ok(())
    }

    /// Admit the provider-preparation boundary and return its exact durable
    /// predecessor. A same-operation renewal proves that the existing marker
    /// remains usable but does not replace it: if an earlier root CAS later
    /// wins, its receipt still names the marker exactly. A higher aggregate
    /// epoch starts a new provider predecessor; shorter, lower, or foreign
    /// requests have zero marker mutation.
    pub(crate) fn prepare_disposal_effect(
        &mut self,
        effect_fence: &pc::SandboxEffectFence,
    ) -> Result<pc::SandboxEffectFence, pc::SandboxError> {
        if self.marker.disposal_authorization.is_some() {
            return Err(err(
                "filesystem realization already entered aggregate-authorized physical authorization",
            ));
        }
        validate_live_effect_fence(effect_fence)?;

        if self
            .marker
            .effect_fence
            .same_realization_lease(effect_fence)
        {
            if !self
                .marker
                .effect_fence
                .authorizes_effect_successor(effect_fence)
            {
                return Err(err(
                    "provider preparation fence is shorter or belongs to another operation",
                ));
            }
            return Ok(self.marker.effect_fence.clone());
        }
        if !self
            .marker
            .effect_fence
            .authorizes_effect_successor(effect_fence)
        {
            return Err(err(
                "provider preparation fence is stale, foreign, or has a lower epoch",
            ));
        }

        let mut next = self.marker.clone();
        next.effect_fence = effect_fence.clone();
        replace_marker_locked(&self.root, &self.lock, &self.marker, &next)?;
        self.marker = next;
        Ok(effect_fence.clone())
    }

    /// Persist or advance the aggregate-derived physical authorization while
    /// retaining its immutable original preparation and fingerprint. The same
    /// marker CAS is the sole Local/Namespace gate: response-loss replay is
    /// exact, a newer authorized successor may take over, and an older or
    /// foreign successor cannot regain destructive authority.
    pub(crate) fn authorize_disposal(
        &mut self,
        authorization: &pc::SandboxDisposalAuthorization,
    ) -> Result<(), pc::SandboxError> {
        validate_live_effect_fence(authorization.effect_fence())?;
        authorization.validate()?;

        let advance = match self.marker.disposal_authorization.as_ref() {
            None => {
                if &self.marker.effect_fence != authorization.prepared_effect_fence() {
                    return Err(err(
                        "physical disposal does not identify the exact prepared aggregate effect",
                    ));
                }
                true
            }
            Some(observed)
                if observed.prepared_effect_fence() != authorization.prepared_effect_fence()
                    || observed.preparation_fingerprint()
                        != authorization.preparation_fingerprint() =>
            {
                return Err(err(
                    "physical disposal conflicts with the immutable aggregate preparation",
                ));
            }
            Some(observed) if observed.effect_fence() == authorization.effect_fence() => false,
            Some(observed)
                if observed
                    .effect_fence()
                    .authorizes_successor(authorization.effect_fence()) =>
            {
                true
            }
            Some(_) => {
                return Err(err(
                    "physical disposal successor is stale or conflicts with a newer takeover",
                ));
            }
        };
        if advance {
            let mut next = self.marker.clone();
            next.disposal_authorization = Some(authorization.clone());
            replace_marker_locked(&self.root, &self.lock, &self.marker, &next)?;
            self.marker = next;
        }
        Ok(())
    }

    pub(crate) fn bind_checkpoint_expected(
        &mut self,
        expected_effect_fence: &pc::SandboxEffectFence,
    ) -> Result<(), pc::SandboxError> {
        expected_effect_fence.validate_identity()?;
        let source = self.marker.terminal_source.as_ref().ok_or_else(|| {
            err("Removing checkpoint participant has no terminal source evidence")
        })?;
        if source.expected_effect_fence.is_some()
            || source
                .realization_effect_fence
                .same_effect_identity(expected_effect_fence)
            || expected_effect_fence.same_effect_identity(&self.marker.effect_fence)
            || !source
                .realization_effect_fence
                .authorizes_successor(expected_effect_fence)
            || !expected_effect_fence.authorizes_successor(&self.marker.effect_fence)
        {
            return Err(err(
                "terminal checkpoint cleanup is not an authorized Suspend continuation",
            ));
        }
        if let Some(recorded) = &self.checkpoint_expected_effect_fence
            && !recorded.same_effect_identity(expected_effect_fence)
        {
            return Err(err(
                "terminal checkpoint cleanup changed its expected source effect",
            ));
        }
        self.checkpoint_expected_effect_fence = Some(expected_effect_fence.clone());
        Ok(())
    }

    fn current_checkpoint_marker(&self) -> Result<RealizationMarker, pc::SandboxError> {
        let expected = self
            .checkpoint_expected_effect_fence
            .as_ref()
            .ok_or_else(|| err("terminal checkpoint cleanup has no bound expected effect"))?;
        validate_live_effect_fence(&self.marker.effect_fence)?;
        let marker = read_marker_locked(&self.root, &self.lock)?
            .ok_or_else(|| err("terminal checkpoint cleanup lost its realization marker"))?;
        let source = marker
            .terminal_source
            .as_ref()
            .ok_or_else(|| err("terminal checkpoint cleanup lost its immutable source evidence"))?;
        if marker.phase != RealizationPhase::Removing
            || !marker
                .effect_fence
                .same_effect_identity(&self.marker.effect_fence)
            || source.expected_effect_fence.is_some()
            || !source
                .realization_effect_fence
                .authorizes_successor(expected)
            || !expected.authorizes_successor(&marker.effect_fence)
            || marker != self.marker
        {
            return Err(err(
                "terminal checkpoint cleanup no longer owns the exact Removing participant",
            ));
        }
        owned_realization_path_locked(&self.root, &self.lock, &marker)?;
        Ok(marker)
    }

    /// Return the exact currently-owned root or private stage. Providers use
    /// this only for pre-delete secret shredding; `None` proves that both names
    /// are absent under the held lifecycle lock.
    pub(crate) fn owned_root(
        &self,
    ) -> Result<Option<(PathBuf, DirectoryIdentity)>, pc::SandboxError> {
        match owned_realization_path_locked(&self.root, &self.lock, &self.marker)? {
            OwnedRealizationPath::Absent => Ok(None),
            OwnedRealizationPath::Exact { path, identity } => Ok(Some((path, identity))),
        }
    }

    pub(crate) fn remove_root(&self) -> Result<(), pc::SandboxError> {
        if self.marker.phase == RealizationPhase::Removed {
            return require_realization_absent(&self.root, &self.lock, &self.marker);
        }
        self.validate_disposal_authorization()?;
        remove_owned_root_or_stage_locked(&self.root, &self.lock, &self.marker)
    }

    pub(crate) fn finish(mut self) -> Result<(), pc::SandboxError> {
        require_realization_absent(&self.root, &self.lock, &self.marker)?;
        if self.marker.phase == RealizationPhase::Removed {
            return Ok(());
        }
        self.validate_disposal_authorization()?;
        let previous = self.marker.clone();
        self.marker.phase = RealizationPhase::Removed;
        replace_marker_locked(&self.root, &self.lock, &previous, &self.marker)
    }
}

/// Stable admission for one non-lifecycle effect against an exact Ready
/// realization. The lifecycle lock is held through the provider's physical
/// snapshot and external mutation so terminal cleanup cannot cross the effect.
pub(crate) struct ReadyOperationGuard {
    root: PathBuf,
    evidence: RealizationEvidence,
    operation_effect_fence: pc::SandboxEffectFence,
    authorization_effect_fence: pc::SandboxEffectFence,
    marker: RealizationMarker,
    lock: awaken_sandbox_fs::ExclusiveFileLock,
}

mod checkpoint_participant;
pub(crate) use checkpoint_participant::CheckpointParticipantGuard;
fn err(error: impl ToString) -> pc::SandboxError {
    pc::SandboxError::new(error.to_string())
}

fn now_unix_ms() -> Result<u64, pc::SandboxError> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| err(format!("read Sandbox effect time: {error}")))?
        .as_millis()
        .try_into()
        .map_err(|_| err("Sandbox effect time exceeds u64"))
}

fn validate_live_effect_fence(fence: &pc::SandboxEffectFence) -> Result<(), pc::SandboxError> {
    fence.validate_live_at(now_unix_ms()?)
}

fn is_distinct_authorized_rebuild(
    observed: &pc::SandboxEffectFence,
    incoming: &pc::SandboxEffectFence,
) -> bool {
    observed.authorizes_successor(incoming) && !observed.same_effect_identity(incoming)
}

mod physical_identity;
use physical_identity::{new_physical_incarnation, validate_removed_restore_source};

mod marker_paths;
use marker_paths::*;
fn classify(path: &Path) -> Result<PathEntry, pc::SandboxError> {
    awaken_sandbox_fs::classify_nofollow(path).map_err(|error| {
        err(format!(
            "inspect sandbox realization path `{}`: {error}",
            path.display()
        ))
    })
}

fn classify_locked(
    lock: &awaken_sandbox_fs::ExclusiveFileLock,
    leaf: &Path,
) -> Result<PathEntry, pc::SandboxError> {
    lock.classify_sibling_nofollow(leaf).map_err(|error| {
        err(format!(
            "inspect locked sandbox realization sibling `{}`: {error}",
            leaf.display()
        ))
    })
}

fn require_absent_locked(
    root: &Path,
    lock: &awaken_sandbox_fs::ExclusiveFileLock,
    leaf: &Path,
    description: &str,
) -> Result<(), pc::SandboxError> {
    match classify_locked(lock, leaf)? {
        PathEntry::Absent => Ok(()),
        _ => Err(err(format!(
            "{description} `{}` is not absent",
            root.parent()
                .unwrap_or_else(|| Path::new(""))
                .join(leaf)
                .display()
        ))),
    }
}

fn require_no_untracked_stage_locked(
    root: &Path,
    lock: &awaken_sandbox_fs::ExclusiveFileLock,
) -> Result<(), pc::SandboxError> {
    let name = root
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| err("sandbox root has no portable final component"))?;
    let prefix = format!(".{name}.awaken-realization-stage-");
    if lock
        .sibling_names()
        .map_err(err)?
        .iter()
        .any(|candidate| candidate.to_string_lossy().starts_with(&prefix))
    {
        Err(err(
            "handle-free terminal cleanup found an untracked private realization stage",
        ))
    } else {
        Ok(())
    }
}

fn require_exact_root_locked(
    root: &Path,
    lock: &awaken_sandbox_fs::ExclusiveFileLock,
    expected: DirectoryIdentity,
) -> Result<(), pc::SandboxError> {
    match classify_locked(lock, root_leaf(root)?)? {
        PathEntry::Directory(observed) if observed == expected => Ok(()),
        PathEntry::Absent => Err(err(format!("sandbox root `{}` is absent", root.display()))),
        _ => Err(err(format!(
            "sandbox root `{}` has a foreign file type or identity",
            root.display()
        ))),
    }
}

fn decode_marker(bytes: &[u8], display: &Path) -> Result<RealizationMarker, pc::SandboxError> {
    let marker: RealizationMarker = serde_json::from_slice(bytes).map_err(|error| {
        err(format!(
            "decode sandbox realization marker `{}`: {error}",
            display.display()
        ))
    })?;
    if marker.schema != MARKER_SCHEMA {
        return Err(err(format!(
            "sandbox realization marker `{}` has unsupported schema {}",
            display.display(),
            marker.schema
        )));
    }
    if marker.physical_incarnation.trim().is_empty() {
        return Err(err(
            "sandbox realization marker has an empty physical incarnation",
        ));
    }
    validate_stage_leaf(&marker.root)?;
    if let Some(completion) = &marker.completion {
        completion.validate()?;
    }
    validate_marker_disposal_authorization(
        marker.phase,
        &marker.effect_fence,
        marker.terminal_source.as_ref(),
        marker.disposal_authorization.as_ref(),
    )?;
    if let Some(source) = &marker.rebuild_source {
        validate_marker_disposal_authorization(
            source.phase,
            &source.effect_fence,
            source.terminal_source.as_ref(),
            source.disposal_authorization.as_ref(),
        )?;
    }
    Ok(marker)
}

fn validate_marker_disposal_authorization(
    phase: RealizationPhase,
    marker_effect_fence: &pc::SandboxEffectFence,
    terminal_source: Option<&TerminalSourceRecord>,
    authorization: Option<&pc::SandboxDisposalAuthorization>,
) -> Result<(), pc::SandboxError> {
    let Some(authorization) = authorization else {
        return Ok(());
    };
    authorization.validate()?;
    marker_effect_fence.validate_identity()?;
    if !matches!(
        phase,
        RealizationPhase::Removing | RealizationPhase::Removed
    ) || terminal_source.is_none()
    {
        return Err(err(
            "filesystem disposal authorization exists outside terminal removal",
        ));
    }

    let prepared = authorization.prepared_effect_fence();
    let latest = authorization.effect_fence();
    // A response-loss replay may observe exactly three legitimate marker
    // cuts: original preparation A, the latest persisted successor, or a newer
    // same-operation successor whose provider-preparation CAS won immediately
    // before the authorization CAS. No unrelated operation may occupy that
    // transient third row.
    if marker_effect_fence != prepared
        && marker_effect_fence != latest
        && !(latest.authorizes_successor(marker_effect_fence)
            && marker_effect_fence.operation_id == latest.operation_id)
    {
        return Err(err(
            "filesystem disposal authorization conflicts with the marker effect",
        ));
    }
    Ok(())
}

fn read_marker_path(path: &Path) -> Result<RealizationMarker, pc::SandboxError> {
    let bytes = awaken_sandbox_fs::read_regular_file_nofollow(path).map_err(|error| {
        err(format!(
            "read sandbox realization marker `{}`: {error}",
            path.display()
        ))
    })?;
    decode_marker(&bytes, path)
}

fn read_marker(root: &Path) -> Result<Option<RealizationMarker>, pc::SandboxError> {
    let path = marker_path(root)?;
    match classify(&path)? {
        PathEntry::Absent => Ok(None),
        PathEntry::RegularFile => read_marker_path(&path).map(Some),
        _ => Err(err(format!(
            "sandbox realization marker `{}` is not a regular file",
            path.display()
        ))),
    }
}

fn read_marker_locked(
    root: &Path,
    lock: &awaken_sandbox_fs::ExclusiveFileLock,
) -> Result<Option<RealizationMarker>, pc::SandboxError> {
    let leaf = marker_leaf(root)?;
    match classify_locked(lock, &leaf)? {
        PathEntry::Absent => Ok(None),
        PathEntry::RegularFile => {
            let bytes = lock
                .read_sibling_regular_file_nofollow(&leaf)
                .map_err(|error| {
                    err(format!(
                        "read locked sandbox realization marker `{}`: {error}",
                        marker_path(root).map_or_else(
                            |_| leaf.display().to_string(),
                            |path| path.display().to_string()
                        )
                    ))
                })?;
            decode_marker(&bytes, &marker_path(root)?).map(Some)
        }
        _ => Err(err(format!(
            "sandbox realization marker `{}` is not a regular file",
            marker_path(root)?.display()
        ))),
    }
}

fn encode_marker(marker: &RealizationMarker) -> Result<Vec<u8>, pc::SandboxError> {
    serde_json::to_vec(marker).map_err(err)
}

fn publish_marker_locked(
    root: &Path,
    lock: &awaken_sandbox_fs::ExclusiveFileLock,
    marker: &RealizationMarker,
) -> Result<(), pc::SandboxError> {
    lock.publish_sibling_file_noreplace(&marker_leaf(root)?, &encode_marker(marker)?)
        .map_err(err)
}

fn replace_marker_locked(
    root: &Path,
    lock: &awaken_sandbox_fs::ExclusiveFileLock,
    expected: &RealizationMarker,
    next: &RealizationMarker,
) -> Result<(), pc::SandboxError> {
    let leaf = marker_leaf(root)?;
    if read_marker_locked(root, lock)? != Some(expected.clone()) {
        return Err(err(
            "sandbox realization marker changed during a locked transition",
        ));
    }
    lock.replace_sibling_regular_file_atomic(&leaf, &encode_marker(next)?)
        .map_err(err)
}

fn remove_marker_exact_locked(
    root: &Path,
    lock: &awaken_sandbox_fs::ExclusiveFileLock,
    expected: &RealizationMarker,
) -> Result<(), pc::SandboxError> {
    let leaf = marker_leaf(root)?;
    if read_marker_locked(root, lock)? != Some(expected.clone()) {
        return Err(err(
            "sandbox realization marker changed before compensation",
        ));
    }
    lock.remove_sibling_regular_file_nofollow(&leaf)
        .map_err(err)
}

fn require_marker_absence_locked(
    root: &Path,
    lock: &awaken_sandbox_fs::ExclusiveFileLock,
) -> Result<(), pc::SandboxError> {
    match read_marker_locked(root, lock)? {
        None => Ok(()),
        Some(_) => Err(err(format!(
            "legacy sandbox handle cannot claim current realization evidence `{}`",
            marker_path(root)?.display()
        ))),
    }
}

fn required_root_identity(
    marker: &RealizationMarker,
) -> Result<DirectoryIdentity, pc::SandboxError> {
    marker
        .root
        .identity
        .map(Into::into)
        .ok_or_else(|| err("sandbox realization marker has no root inode evidence"))
}

fn evidence(marker: &RealizationMarker) -> Result<RealizationEvidence, pc::SandboxError> {
    Ok(RealizationEvidence {
        fingerprint: marker.fingerprint.clone(),
        effect_fence: marker.effect_fence.clone(),
        physical_incarnation: marker.physical_incarnation.clone(),
        root_identity: required_root_identity(marker)?,
    })
}

fn root_record(root: &Path, incarnation: &str) -> Result<RootRecord, pc::SandboxError> {
    Ok(RootRecord {
        private_stage_leaf: private_stage_leaf(root, incarnation)?,
        identity: None,
        published: false,
    })
}

/// Materialize or recover the exact private-stage -> final-root publication.
/// The marker is always published before the root; after inode capture it holds
/// both the private leaf and identity, closing the response-loss cut between
/// marker publication and the no-replace rename.
fn ensure_root_published(
    root: &Path,
    lock: &awaken_sandbox_fs::ExclusiveFileLock,
    marker: &mut RealizationMarker,
) -> Result<(), pc::SandboxError> {
    let stage = stage_path(root, &marker.root)?;
    let stage_leaf = stage_leaf(&marker.root)?.to_path_buf();
    let root_leaf = root_leaf(root)?;
    if marker.root.identity.is_none() {
        require_absent_locked(root, lock, root_leaf, "sandbox root")?;
        match classify_locked(lock, &stage_leaf)? {
            PathEntry::Absent => {}
            _ => {
                return Err(err(format!(
                    "sandbox private stage `{}` exists without durable inode evidence",
                    stage.display()
                )));
            }
        }
        validate_live_effect_fence(&marker.effect_fence)?;
        let previous = marker.clone();
        marker.root.identity = Some(
            lock.create_sibling_directory_noreplace(&stage_leaf)
                .map_err(err)?
                .into(),
        );
        // Marker replacement failure leaves no durable identity that could
        // authorize cleanup; preserve the stage and fail closed.
        replace_marker_locked(root, lock, &previous, marker)?;
    }

    let identity = required_root_identity(marker)?;
    let root_entry = classify_locked(lock, root_leaf)?;
    let stage_entry = classify_locked(lock, &stage_leaf)?;
    match (marker.root.published, root_entry, stage_entry) {
        (false, PathEntry::Absent, PathEntry::Directory(observed)) if observed == identity => {
            validate_live_effect_fence(&marker.effect_fence)?;
            lock.publish_sibling_directory_noreplace(&stage_leaf, root_leaf)
                .map_err(err)?;
            let previous = marker.clone();
            marker.root.published = true;
            replace_marker_locked(root, lock, &previous, marker)
        }
        // Rename succeeded and the process died before publishing the phase bit.
        (false, PathEntry::Directory(observed), PathEntry::Absent) if observed == identity => {
            validate_live_effect_fence(&marker.effect_fence)?;
            let previous = marker.clone();
            marker.root.published = true;
            replace_marker_locked(root, lock, &previous, marker)
        }
        (true, PathEntry::Directory(observed), PathEntry::Absent) if observed == identity => Ok(()),
        _ => Err(err(format!(
            "sandbox root or private stage was substituted for incarnation `{}`",
            marker.physical_incarnation
        ))),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum OwnedRealizationPath {
    Absent,
    Exact {
        path: PathBuf,
        identity: DirectoryIdentity,
    },
}

/// Classify both names that can own one physical incarnation. Looking only at
/// the phase bit is insufficient: `rename(stage, root)` can succeed immediately
/// before the marker records `published = true`.
fn owned_realization_path(
    root: &Path,
    marker: &RealizationMarker,
) -> Result<OwnedRealizationPath, pc::SandboxError> {
    let stage = stage_path(root, &marker.root)?;
    let root_entry = classify(root)?;
    let stage_entry = classify(&stage)?;
    owned_realization_path_from_entries(root, marker, stage, root_entry, stage_entry)
}

fn owned_realization_path_locked(
    root: &Path,
    lock: &awaken_sandbox_fs::ExclusiveFileLock,
    marker: &RealizationMarker,
) -> Result<OwnedRealizationPath, pc::SandboxError> {
    let stage = stage_path(root, &marker.root)?;
    let root_entry = classify_locked(lock, root_leaf(root)?)?;
    let stage_entry = classify_locked(lock, stage_leaf(&marker.root)?)?;
    owned_realization_path_from_entries(root, marker, stage, root_entry, stage_entry)
}

fn owned_realization_path_from_entries(
    root: &Path,
    marker: &RealizationMarker,
    stage: PathBuf,
    root_entry: PathEntry,
    stage_entry: PathEntry,
) -> Result<OwnedRealizationPath, pc::SandboxError> {
    let Some(identity) = marker.root.identity.map(Into::into) else {
        return match (root_entry, stage_entry) {
            (PathEntry::Absent, PathEntry::Absent) => Ok(OwnedRealizationPath::Absent),
            _ => Err(err(
                "sandbox realization has a physical path without durable inode evidence",
            )),
        };
    };
    match (root_entry, stage_entry) {
        (PathEntry::Absent, PathEntry::Absent) => Ok(OwnedRealizationPath::Absent),
        (PathEntry::Directory(observed), PathEntry::Absent) if observed == identity => {
            Ok(OwnedRealizationPath::Exact {
                path: root.to_owned(),
                identity,
            })
        }
        (PathEntry::Absent, PathEntry::Directory(observed))
            if observed == identity && !marker.root.published =>
        {
            Ok(OwnedRealizationPath::Exact {
                path: stage,
                identity,
            })
        }
        _ => Err(err(format!(
            "sandbox root or private stage was substituted for incarnation `{}`",
            marker.physical_incarnation
        ))),
    }
}

fn require_realization_absent(
    root: &Path,
    lock: &awaken_sandbox_fs::ExclusiveFileLock,
    marker: &RealizationMarker,
) -> Result<(), pc::SandboxError> {
    match owned_realization_path_locked(root, lock, marker)? {
        OwnedRealizationPath::Absent => Ok(()),
        OwnedRealizationPath::Exact { path, .. } => Err(err(format!(
            "terminal sandbox realization `{}` is not absent",
            path.display()
        ))),
    }
}

fn remove_owned_root_or_stage_locked(
    root: &Path,
    lock: &awaken_sandbox_fs::ExclusiveFileLock,
    marker: &RealizationMarker,
) -> Result<(), pc::SandboxError> {
    match owned_realization_path_locked(root, lock, marker)? {
        OwnedRealizationPath::Absent => Ok(()),
        OwnedRealizationPath::Exact { path, identity } => {
            let leaf = if path == root {
                root_leaf(root)?
            } else {
                stage_leaf(&marker.root)?
            };
            lock.remove_sibling_directory_tree_exact(leaf, identity)
                .map_err(err)
        }
    }
}

fn source_matches(marker: &RealizationMarker, source: &RebuildSource<'_>) -> bool {
    marker.fingerprint == *source.fingerprint
        && marker.physical_incarnation == source.physical_incarnation
        && marker
            .effect_fence
            .same_effect_identity(source.effect_fence)
}

fn marker_for_new_attempt(
    root: &Path,
    fingerprint: &pc::SandboxRealizationFingerprint,
    fence: &pc::SandboxEffectFence,
    phase: RealizationPhase,
    rebuild_source: Option<RebuildSourceRecord>,
    restore_input_fingerprint: Option<&str>,
) -> Result<RealizationMarker, pc::SandboxError> {
    let incarnation = new_physical_incarnation()?;
    Ok(RealizationMarker {
        schema: MARKER_SCHEMA,
        fingerprint: fingerprint.clone(),
        effect_fence: fence.clone(),
        physical_incarnation: incarnation.clone(),
        phase,
        root: root_record(root, &incarnation)?,
        rebuild_source,
        checkpoint: None,
        terminal_source: None,
        disposal_authorization: None,
        restore_input_fingerprint: restore_input_fingerprint.map(str::to_owned),
        completion: None,
    })
}

/// Begin a fenced create or exact-source rebuild.
pub(crate) fn begin(
    root: &Path,
    fingerprint: &pc::SandboxRealizationFingerprint,
    effect_fence: &pc::SandboxEffectFence,
    source: Option<RebuildSource<'_>>,
    restore_input_fingerprint: Option<&str>,
) -> Result<CreationGuard, pc::SandboxError> {
    effect_fence.validate_identity()?;
    let lock = acquire(root)?;
    let Some(mut observed) = read_marker_locked(root, &lock)? else {
        validate_live_effect_fence(effect_fence)?;
        if source.is_some() {
            return Err(err(
                "sandbox rebuild source has no provider realization marker",
            ));
        }
        require_absent_locked(root, &lock, root_leaf(root)?, "sandbox root")?;
        let mut marker = marker_for_new_attempt(
            root,
            fingerprint,
            effect_fence,
            RealizationPhase::Creating,
            None,
            restore_input_fingerprint,
        )?;
        require_absent_locked(
            root,
            &lock,
            stage_leaf(&marker.root)?,
            "sandbox private stage",
        )?;
        publish_marker_locked(root, &lock, &marker)?;
        ensure_root_published(root, &lock, &mut marker)?;
        return Ok(CreationGuard {
            root: root.to_owned(),
            marker,
            admission: Admission::Creating,
            lock: Some(lock),
        });
    };

    if observed.fingerprint != *fingerprint {
        return Err(err(
            "sandbox realization belongs to a different immutable specification",
        ));
    }
    match observed.phase {
        RealizationPhase::Creating => {
            validate_live_effect_fence(effect_fence)?;
            if source.is_some()
                || !observed.effect_fence.same_effect_identity(effect_fence)
                || observed.restore_input_fingerprint.as_deref() != restore_input_fingerprint
            {
                return Err(err("incomplete sandbox creation belongs to another effect"));
            }
            if effect_fence.expires_at_unix_ms > observed.effect_fence.expires_at_unix_ms {
                let previous = observed.clone();
                observed.effect_fence = effect_fence.clone();
                replace_marker_locked(root, &lock, &previous, &observed)?;
            }
            ensure_root_published(root, &lock, &mut observed)?;
            Ok(CreationGuard {
                root: root.to_owned(),
                marker: observed,
                admission: Admission::Creating,
                lock: Some(lock),
            })
        }
        RealizationPhase::Recreating => {
            validate_live_effect_fence(effect_fence)?;
            let recorded = observed
                .rebuild_source
                .as_ref()
                .ok_or_else(|| err("recreating sandbox has no source evidence"))?;
            if recorded.phase == RealizationPhase::Removed {
                let restore_input = restore_input_fingerprint.ok_or_else(|| {
                    err("checkpoint restore retry has no exact restore input fingerprint")
                })?;
                if source.is_some()
                    || !observed.effect_fence.same_effect_identity(effect_fence)
                    || observed.restore_input_fingerprint.as_deref() != Some(restore_input)
                {
                    return Err(err(
                        "checkpoint restore retry conflicts with its Removed predecessor",
                    ));
                }
                validate_removed_restore_source(recorded, restore_input)?;
            } else {
                let source = source
                    .ok_or_else(|| err("recreating sandbox requires its exact source handle"))?;
                if recorded.physical_incarnation != source.physical_incarnation
                    || !recorded
                        .effect_fence
                        .same_effect_identity(source.effect_fence)
                    || !observed.effect_fence.same_effect_identity(effect_fence)
                    || observed.fingerprint != *source.fingerprint
                    || observed.restore_input_fingerprint.as_deref() != restore_input_fingerprint
                {
                    return Err(err(
                        "recreating sandbox source or effect evidence conflicts",
                    ));
                }
            }
            if effect_fence.expires_at_unix_ms > observed.effect_fence.expires_at_unix_ms {
                let previous = observed.clone();
                observed.effect_fence = effect_fence.clone();
                replace_marker_locked(root, &lock, &previous, &observed)?;
            }
            ensure_root_published(root, &lock, &mut observed)?;
            Ok(CreationGuard {
                root: root.to_owned(),
                marker: observed,
                admission: Admission::Recreating,
                lock: Some(lock),
            })
        }
        RealizationPhase::Ready => match classify_locked(&lock, root_leaf(root)?)? {
            PathEntry::Directory(identity) if Some(identity.into()) == observed.root.identity => {
                if source.is_none()
                    && observed.effect_fence.same_effect_identity(effect_fence)
                    && observed.restore_input_fingerprint.as_deref() == restore_input_fingerprint
                {
                    Ok(CreationGuard {
                        root: root.to_owned(),
                        marker: observed,
                        admission: Admission::Ready,
                        lock: Some(lock),
                    })
                } else {
                    Err(err(
                        "ready sandbox root conflicts with another create or rebuild effect",
                    ))
                }
            }
            PathEntry::Absent => {
                validate_live_effect_fence(effect_fence)?;
                let source = source.ok_or_else(|| {
                    err("missing ready sandbox requires an exact V2 rebuild source handle")
                })?;
                if !source_matches(&observed, &source)
                    || !is_distinct_authorized_rebuild(&observed.effect_fence, effect_fence)
                {
                    return Err(err(
                        "sandbox rebuild source or authorization is stale/conflicting",
                    ));
                }
                let rebuild_source = RebuildSourceRecord {
                    phase: RealizationPhase::Ready,
                    effect_fence: observed.effect_fence.clone(),
                    physical_incarnation: observed.physical_incarnation.clone(),
                    root: observed.root.clone(),
                    checkpoint: observed.checkpoint.clone(),
                    terminal_source: observed.terminal_source.clone(),
                    disposal_authorization: observed.disposal_authorization.clone(),
                    restore_input_fingerprint: observed.restore_input_fingerprint.clone(),
                    completion: observed.completion.clone(),
                };
                let mut rebuilding = marker_for_new_attempt(
                    root,
                    fingerprint,
                    effect_fence,
                    RealizationPhase::Recreating,
                    Some(rebuild_source),
                    restore_input_fingerprint,
                )?;
                require_absent_locked(
                    root,
                    &lock,
                    stage_leaf(&rebuilding.root)?,
                    "sandbox rebuild private stage",
                )?;
                replace_marker_locked(root, &lock, &observed, &rebuilding)?;
                ensure_root_published(root, &lock, &mut rebuilding)?;
                Ok(CreationGuard {
                    root: root.to_owned(),
                    marker: rebuilding,
                    admission: Admission::Recreating,
                    lock: Some(lock),
                })
            }
            _ => Err(err(
                "ready sandbox root has a foreign type or physical identity",
            )),
        },
        RealizationPhase::Removed => {
            validate_live_effect_fence(effect_fence)?;
            if source.is_some() {
                return Err(err(
                    "Removed filesystem restoration does not accept a stale source handle",
                ));
            }
            let restore_input = restore_input_fingerprint.ok_or_else(|| {
                err("Removed filesystem can only transition through an exact checkpoint restore")
            })?;
            if !is_distinct_authorized_rebuild(&observed.effect_fence, effect_fence) {
                return Err(err(
                    "checkpoint restore fence does not authorize the Removed predecessor",
                ));
            }
            require_realization_absent(root, &lock, &observed)?;
            let rebuild_source = RebuildSourceRecord {
                phase: RealizationPhase::Removed,
                effect_fence: observed.effect_fence.clone(),
                physical_incarnation: observed.physical_incarnation.clone(),
                root: observed.root.clone(),
                checkpoint: observed.checkpoint.clone(),
                terminal_source: observed.terminal_source.clone(),
                disposal_authorization: observed.disposal_authorization.clone(),
                restore_input_fingerprint: observed.restore_input_fingerprint.clone(),
                completion: observed.completion.clone(),
            };
            validate_removed_restore_source(&rebuild_source, restore_input)?;
            let mut restoring = marker_for_new_attempt(
                root,
                fingerprint,
                effect_fence,
                RealizationPhase::Recreating,
                Some(rebuild_source),
                Some(restore_input),
            )?;
            require_absent_locked(
                root,
                &lock,
                stage_leaf(&restoring.root)?,
                "checkpoint restore private stage",
            )?;
            replace_marker_locked(root, &lock, &observed, &restoring)?;
            ensure_root_published(root, &lock, &mut restoring)?;
            Ok(CreationGuard {
                root: root.to_owned(),
                marker: restoring,
                admission: Admission::Recreating,
                lock: Some(lock),
            })
        }
        RealizationPhase::Removing => Err(err(
            "terminal sandbox realization cannot be created or rebuilt",
        )),
    }
}

pub(crate) fn begin_legacy(root: &Path) -> Result<LegacyCreationGuard, pc::SandboxError> {
    let lock = acquire(root)?;
    require_marker_absence_locked(root, &lock)?;
    require_absent_locked(root, &lock, root_leaf(root)?, "legacy sandbox root")?;
    Ok(LegacyCreationGuard {
        root: root.to_owned(),
        identity: None,
        lock: Some(lock),
    })
}

mod observation;
use observation::validate_handle_marker;
pub(crate) use observation::{observe_adoption, verify_adoption};

fn validate_ready_operation_marker(
    root: &Path,
    marker: &RealizationMarker,
    evidence: &RealizationEvidence,
    operation_effect_fence: &pc::SandboxEffectFence,
    authorization_effect_fence: &pc::SandboxEffectFence,
    marker_effect_fence: &pc::SandboxEffectFence,
    lock: &awaken_sandbox_fs::ExclusiveFileLock,
) -> Result<(), pc::SandboxError> {
    if marker.phase != RealizationPhase::Ready
        || marker.fingerprint != evidence.fingerprint
        || marker.physical_incarnation != evidence.physical_incarnation
        || required_root_identity(marker)? != evidence.root_identity
        || !marker
            .effect_fence
            .same_effect_identity(&evidence.effect_fence)
        || !marker
            .effect_fence
            .same_effect_identity(marker_effect_fence)
        || !marker
            .effect_fence
            .authorizes_successor(operation_effect_fence)
        || !marker
            .effect_fence
            .authorizes_successor(authorization_effect_fence)
        || !operation_effect_fence.authorizes_successor(authorization_effect_fence)
    {
        return Err(err(
            "ready operation does not identify the current filesystem realization",
        ));
    }
    require_exact_root_locked(root, lock, evidence.root_identity)
}

/// Admit a non-lifecycle effect through the same marker lock and immutable
/// realization evidence as create/adopt/dispose. No marker phase or parallel
/// operation ledger is introduced: Session remains the effect authority and
/// this guard only proves that its live fence authorizes the exact Ready root.
pub(crate) fn begin_ready_operation(
    root: &Path,
    evidence: &RealizationEvidence,
    effect_fence: &pc::SandboxEffectFence,
) -> Result<ReadyOperationGuard, pc::SandboxError> {
    begin_ready_operation_with_authorization(root, evidence, effect_fence, effect_fence)
}

/// Admit terminal recovery of an in-flight checkpoint while its live resident
/// still owns the Ready realization. The Suspend fence identifies the WAL
/// participant; the distinct terminal fence authorizes every mutation. This is
/// the Ready lock entrance to the same [`CheckpointParticipantGuard`] used by
/// cold Removing recovery, not a second lifecycle or checkpoint algorithm.
pub(crate) fn begin_ready_terminal_checkpoint(
    root: &Path,
    evidence: &RealizationEvidence,
    expected_effect_fence: &pc::SandboxEffectFence,
    terminal_effect_fence: &pc::SandboxEffectFence,
) -> Result<ReadyOperationGuard, pc::SandboxError> {
    if evidence
        .effect_fence
        .same_effect_identity(expected_effect_fence)
        || expected_effect_fence.same_effect_identity(terminal_effect_fence)
    {
        return Err(err(
            "terminal checkpoint cleanup requires distinct realization, Suspend, and terminal effects",
        ));
    }
    begin_ready_operation_with_authorization(
        root,
        evidence,
        expected_effect_fence,
        terminal_effect_fence,
    )
}

fn begin_ready_operation_with_authorization(
    root: &Path,
    evidence: &RealizationEvidence,
    operation_effect_fence: &pc::SandboxEffectFence,
    authorization_effect_fence: &pc::SandboxEffectFence,
) -> Result<ReadyOperationGuard, pc::SandboxError> {
    operation_effect_fence.validate_identity()?;
    authorization_effect_fence.validate_identity()?;
    // Admission and receipt lookup are read-only; expiry is checked at every
    // WAL/store mutation boundary by the guard methods below.
    let lock = acquire(root)?;
    let marker = read_marker_locked(root, &lock)?
        .ok_or_else(|| err("ready filesystem operation has no realization marker"))?;
    validate_ready_operation_marker(
        root,
        &marker,
        evidence,
        operation_effect_fence,
        authorization_effect_fence,
        &marker.effect_fence,
        &lock,
    )?;
    Ok(ReadyOperationGuard {
        root: root.to_owned(),
        evidence: evidence.clone(),
        operation_effect_fence: operation_effect_fence.clone(),
        authorization_effect_fence: authorization_effect_fence.clone(),
        marker,
        lock,
    })
}

/// Enter the terminal edge of the one filesystem-realization lifecycle. A V2
/// handle binds a published physical incarnation; an in-flight restore may lack
/// that handle and must instead present the exact effect that created the
/// marker. The returned guard owns the lifecycle lock through shred/delete and
/// tombstone publication. `None` means both physical names are proven absent
/// and the marker is absent or durably `Removed`.
pub(crate) fn begin_terminal_takeover(
    root: &Path,
    fingerprint: &pc::SandboxRealizationFingerprint,
    handle: Option<RebuildSource<'_>>,
    expected_effect_fence: Option<&pc::SandboxEffectFence>,
    terminal_effect_fence: &pc::SandboxEffectFence,
) -> Result<Option<(RealizationEvidence, RemovalGuard)>, pc::SandboxError> {
    validate_live_effect_fence(terminal_effect_fence)?;
    if let Some(expected) = expected_effect_fence {
        expected.validate_identity()?;
    }
    let lock = acquire(root)?;
    let Some(mut marker) = read_marker_locked(root, &lock)? else {
        let (source_fence, incarnation) = match handle {
            Some(source) => {
                source.effect_fence.validate_identity()?;
                if source.fingerprint != fingerprint
                    || source.physical_incarnation.trim().is_empty()
                {
                    return Err(err(
                        "terminal handle does not bind the absent filesystem realization",
                    ));
                }
                (
                    source.effect_fence,
                    Some(source.physical_incarnation.to_owned()),
                )
            }
            None => {
                let expected = expected_effect_fence.ok_or_else(|| {
                    err("handle-free terminal cleanup requires its exact restore effect fence")
                })?;
                (expected, None)
            }
        };
        if !source_fence.authorizes_successor(terminal_effect_fence)
            || expected_effect_fence
                .is_some_and(|expected| !source_fence.same_effect_identity(expected))
            || expected_effect_fence
                .is_some_and(|expected| !expected.authorizes_successor(terminal_effect_fence))
        {
            return Err(err(
                "terminal fence does not authorize the absent realization evidence",
            ));
        }
        require_absent_locked(root, &lock, root_leaf(root)?, "terminal sandbox root")?;
        if let Some(incarnation) = incarnation {
            let record = root_record(root, &incarnation)?;
            require_absent_locked(
                root,
                &lock,
                stage_leaf(&record)?,
                "terminal sandbox private stage",
            )?;
        } else {
            require_no_untracked_stage_locked(root, &lock)?;
        }
        return Ok(None);
    };
    if marker.fingerprint != *fingerprint {
        return Err(err(
            "terminal cleanup specification does not identify the realization marker",
        ));
    }

    let existing_terminal_source = marker.terminal_source.clone();
    let terminal_source = match marker.phase {
        RealizationPhase::Removing | RealizationPhase::Removed => {
            let recorded = existing_terminal_source.ok_or_else(|| {
                err("terminal realization marker has no immutable source evidence")
            })?;
            match &handle {
                Some(source) => {
                    validate_handle_marker(
                        &marker,
                        source.fingerprint,
                        source.effect_fence,
                        source.physical_incarnation,
                    )?;
                    if !recorded
                        .realization_effect_fence
                        .same_effect_identity(source.effect_fence)
                    {
                        return Err(err(
                            "terminal handle conflicts with the recorded physical source",
                        ));
                    }
                }
                None => {
                    let expected = expected_effect_fence.ok_or_else(|| {
                        err("handle-free terminal replay requires its exact restore effect")
                    })?;
                    if !recorded
                        .realization_effect_fence
                        .same_effect_identity(expected)
                    {
                        return Err(err(
                            "handle-free terminal replay does not identify the restore source",
                        ));
                    }
                }
            }
            let expected_matches = match (
                recorded.expected_effect_fence.as_ref(),
                expected_effect_fence,
            ) {
                (None, None) => true,
                (Some(recorded), Some(expected)) => recorded.same_effect_identity(expected),
                _ => false,
            };
            if !expected_matches {
                return Err(err(
                    "terminal replay changed the expected in-flight operation fence",
                ));
            }
            recorded
        }
        RealizationPhase::Creating | RealizationPhase::Recreating | RealizationPhase::Ready => {
            let realization_effect_fence = match &handle {
                Some(source) => {
                    validate_handle_marker(
                        &marker,
                        source.fingerprint,
                        source.effect_fence,
                        source.physical_incarnation,
                    )?;
                    source.effect_fence.clone()
                }
                None => {
                    let expected = expected_effect_fence.ok_or_else(|| {
                        err("handle-free terminal cleanup requires its exact restore effect")
                    })?;
                    if !marker.effect_fence.same_effect_identity(expected) {
                        return Err(err(
                            "handle-free terminal cleanup does not identify the in-flight restore",
                        ));
                    }
                    expected.clone()
                }
            };
            if let Some(expected) = expected_effect_fence
                && !realization_effect_fence.same_effect_identity(expected)
            {
                return Err(err(
                    "expected operation fence does not exactly identify the physical source effect",
                ));
            }
            TerminalSourceRecord {
                realization_effect_fence,
                expected_effect_fence: expected_effect_fence.cloned(),
            }
        }
    };

    if !terminal_source
        .realization_effect_fence
        .authorizes_successor(terminal_effect_fence)
        || terminal_source
            .expected_effect_fence
            .as_ref()
            .is_some_and(|expected| !expected.authorizes_successor(terminal_effect_fence))
    {
        return Err(err(
            "terminal fence does not authorize the exact physical or in-flight source",
        ));
    }

    let owned = owned_realization_path_locked(root, &lock, &marker)?;
    if marker.phase == RealizationPhase::Removed {
        return match owned {
            OwnedRealizationPath::Absent => Ok(None),
            OwnedRealizationPath::Exact { .. } => Err(err(
                "removed filesystem realization still has a physical participant",
            )),
        };
    }
    if marker.phase == RealizationPhase::Ready
        && matches!(&owned, OwnedRealizationPath::Exact { path, .. } if path != root)
    {
        return Err(err(
            "ready filesystem realization is stranded at its private stage",
        ));
    }

    if marker.phase != RealizationPhase::Removing {
        let previous = marker.clone();
        marker.phase = RealizationPhase::Removing;
        marker.effect_fence = terminal_effect_fence.clone();
        marker.terminal_source = Some(terminal_source.clone());
        replace_marker_locked(root, &lock, &previous, &marker)?;
    } else if !marker
        .effect_fence
        .authorizes_successor(terminal_effect_fence)
    {
        return Err(err("terminal cleanup fence is stale or foreign"));
    }

    // Reopening an existing Removing participant is effect-free. In
    // particular, terminal takeover of a continuation-prepared source must
    // retain predecessor A for the later aggregate-derived A/fingerprint->C
    // authorization. Provider preparation is admitted through
    // `RemovalGuard::prepare_disposal_effect`; physical authorization advances
    // only its typed `disposal_authorization` below.

    let identity = match owned {
        OwnedRealizationPath::Exact { identity, .. } => identity,
        OwnedRealizationPath::Absent if marker.root.identity.is_some() => {
            required_root_identity(&marker)?
        }
        OwnedRealizationPath::Absent => {
            validate_live_effect_fence(&marker.effect_fence)?;
            let previous = marker.clone();
            marker.phase = RealizationPhase::Removed;
            replace_marker_locked(root, &lock, &previous, &marker)?;
            return Ok(None);
        }
    };
    let realization = RealizationEvidence {
        fingerprint: marker.fingerprint.clone(),
        effect_fence: terminal_source.realization_effect_fence,
        physical_incarnation: marker.physical_incarnation.clone(),
        root_identity: identity,
    };
    Ok(Some((
        realization,
        RemovalGuard {
            root: root.to_owned(),
            marker,
            checkpoint_expected_effect_fence: None,
            lock,
        },
    )))
}

/// Delete only a root whose one-shot creator still holds its in-memory inode
/// evidence. Marker-free V1 adoption passes `None` and is non-destructive.
pub(crate) fn dispose_legacy(
    root: &Path,
    live_identity: Option<DirectoryIdentity>,
) -> Result<(), pc::SandboxError> {
    let Some(identity) = live_identity else {
        return Err(err(
            "legacy adopted sandbox has no destructive root evidence",
        ));
    };
    let lock = acquire(root)?;
    require_marker_absence_locked(root, &lock)?;
    match classify_locked(&lock, root_leaf(root)?)? {
        PathEntry::Absent => Ok(()),
        PathEntry::Directory(observed) if observed == identity => lock
            .remove_sibling_directory_tree_exact(root_leaf(root)?, identity)
            .map_err(err),
        _ => Err(err("legacy sandbox root was substituted before disposal")),
    }
}

#[cfg(test)]
#[path = "realization_marker/tests.rs"]
mod tests;
