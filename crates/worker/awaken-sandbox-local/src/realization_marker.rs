// @formal surface requirement=runtime.capabilities.capability_floor
// @formal reason: Static owner: the single crash-safe filesystem realization marker shared by Local and
// @formal reason: Namespace providers. Dynamic path: one locked marker advances create, rebuild, ready,
// @formal reason: terminal removal, and removed cuts while binding the exact effect fence, immutable
// @formal reason: fingerprint, physical incarnation, root inode, checkpoint participant, and completion
// @formal reason: receipt; Session authorization remains upstream and this is not a direct formal-proof
// @formal reason: claim.
//! One crash-safe filesystem-realization authority shared by Local and
//! Namespace sandbox adapters.
//!
//! Session owns authorization. This provider sidecar persists only the exact
//! projected effect fence, immutable spec, physical incarnation, root inode,
//! and lifecycle crash cut needed to apply that authorization safely.
//! Creation's provider guard borrows the captured current authority around
//! these physical transitions; this marker does not compare Control timestamps
//! against a second Worker clock.

use std::path::{Component, Path, PathBuf};

use crate::FilesystemHandleCodec as _;
use awaken_provisioning_contract as pc;
use awaken_sandbox_fs::{DirectoryIdentity, PathEntry};
use pc::FenceToken;
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
    checkpoint: Option<pc::SandboxCheckpointUpload>,
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
    /// Physical process participants, written before GO under this marker's
    /// existing lock. Session remains the sole execution authority. Old schema-2
    /// markers have no such participants and retain their weaker guarantees.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    executions: Vec<awaken_local_process::ProcessExecutionIdentity>,
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
    checkpoint: Option<pc::SandboxCheckpointUpload>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    terminal_source: Option<TerminalSourceRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    disposal_authorization: Option<pc::SandboxDisposalAuthorization>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    restore_input_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    completion: Option<RealizationCompletionReceipt>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    executions: Vec<awaken_local_process::ProcessExecutionIdentity>,
}

fn ready_phase() -> RealizationPhase {
    RealizationPhase::Ready
}

mod receipt;
pub(crate) use receipt::RealizationCompletionReceipt;
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
    pub(crate) physical_incarnation: String,
}

pub(crate) fn rebuild_source(
    handle: &pc::SandboxHandle,
) -> Result<RebuildSource<'_>, pc::SandboxError> {
    Ok(RebuildSource {
        fingerprint: handle.realization_fingerprint().ok_or_else(|| {
            pc::SandboxError::unsupported("legacy sandbox handle cannot authorize rebuild")
        })?,
        effect_fence: handle.filesystem_effect_fence()?.ok_or_else(|| {
            pc::SandboxError::unsupported("legacy sandbox handle has no rebuild effect fence")
        })?,
        physical_incarnation: handle.filesystem_physical_incarnation()?.ok_or_else(|| {
            pc::SandboxError::unsupported(
                "legacy sandbox handle has no physical rebuild incarnation",
            )
        })?,
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
            _ => Err(pc::SandboxError::conflict(format!(
                "sandbox root `{}` has a foreign file type or physical identity",
                root.display()
            ))),
        }
    }

    pub(crate) fn require_root_identity(
        &self,
        root: &Path,
    ) -> Result<DirectoryIdentity, pc::SandboxError> {
        self.root_identity_for_access(root)?.ok_or_else(|| {
            pc::SandboxError::not_found(format!("sandbox root `{}` is absent", root.display()))
        })
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
    /// The admitted effect must be exactly the effect this held marker records.
    /// Physical creation mutations take the guard so they cannot run outside
    /// the current authority's critical section; the marker CAS below remains
    /// the backend linearization point.
    fn require_admitted(
        &self,
        admitted: &pc::SandboxEffectGuard<'_>,
    ) -> Result<(), pc::SandboxError> {
        admitted.require_effect(&self.marker.effect_fence)
    }

    /// Test fixture: admit this held creation under its own recorded effect.
    #[cfg(test)]
    pub(crate) fn recorded_admission<T>(
        &mut self,
        apply: impl FnOnce(&mut Self, &pc::SandboxEffectGuard<'_>) -> Result<T, pc::SandboxError>,
    ) -> Result<T, pc::SandboxError> {
        let fence = self.marker.effect_fence.clone();
        pc::with_current_sandbox_effect(
            &fence,
            &pc::test_support::EffectAuthority::new(&fence),
            |admitted| apply(self, admitted),
        )
    }

    fn lock(&self) -> Result<&awaken_sandbox_fs::ExclusiveFileLock, pc::SandboxError> {
        self.lock.as_ref().ok_or_else(|| {
            pc::SandboxError::conflict("filesystem creation lifecycle lock was released")
        })
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
            .ok_or_else(|| {
                pc::SandboxError::not_found(
                    "Ready filesystem realization has no exact completion receipt",
                )
            })
            .map(Some)
    }

    pub(crate) fn validate_before_mutation(&self) -> Result<(), pc::SandboxError> {
        let observed = read_marker_locked(&self.root, self.lock()?)?.ok_or_else(|| {
            pc::SandboxError::gone("filesystem creation lost its realization marker")
        })?;
        if observed != self.marker {
            return Err(pc::SandboxError::conflict(
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
    pub(crate) fn prepare_root(
        &self,
        admitted: &pc::SandboxEffectGuard<'_>,
    ) -> Result<(), pc::SandboxError> {
        self.require_admitted(admitted)?;
        match self.admission {
            Admission::Creating | Admission::Recreating => {
                let identity = required_root_identity(&self.marker)?;
                require_exact_root_locked(&self.root, self.lock()?, identity)?;
                self.lock()?
                    .clear_sibling_directory_contents_exact(root_leaf(&self.root)?, identity)
                    .map_err(|error| pc::SandboxError::backend("prepare_root", error))
            }
            Admission::Ready => Ok(()),
        }
    }

    /// Publish Ready only after all provider materialization succeeds.
    pub(crate) fn complete(
        &mut self,
        receipt: &RealizationCompletionReceipt,
        admitted: &pc::SandboxEffectGuard<'_>,
    ) -> Result<RealizationEvidence, pc::SandboxError> {
        self.require_admitted(admitted)?;
        receipt.validate()?;
        let identity = required_root_identity(&self.marker)?;
        require_exact_root_locked(&self.root, self.lock()?, identity)?;
        if self.admission == Admission::Ready {
            let observed = read_marker_locked(&self.root, self.lock()?)?.ok_or_else(|| {
                pc::SandboxError::gone("Ready filesystem replay lost its realization marker")
            })?;
            if observed != self.marker || self.marker.completion.as_ref() != Some(receipt) {
                return Err(pc::SandboxError::conflict(
                    "Ready filesystem response-loss replay changed its completion receipt",
                ));
            }
        } else {
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
    pub(crate) fn abort_creation(
        self,
        admitted: &pc::SandboxEffectGuard<'_>,
    ) -> Result<(), pc::SandboxError> {
        if self.admission == Admission::Ready {
            return Ok(());
        }
        self.require_admitted(admitted)?;
        remove_owned_root_or_stage_locked(&self.root, self.lock()?, &self.marker)?;
        match self.marker.phase {
            RealizationPhase::Creating => {
                remove_marker_exact_locked(&self.root, self.lock()?, &self.marker)
            }
            RealizationPhase::Recreating => {
                let source = self.marker.rebuild_source.as_ref().ok_or_else(|| {
                    pc::SandboxError::not_found(
                        "recreating sandbox marker has no exact rebuild source",
                    )
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
                    executions: source.executions.clone(),
                };
                replace_marker_locked(&self.root, self.lock()?, &self.marker, &restored)
            }
            _ => Err(pc::SandboxError::conflict(
                "sandbox creation guard entered a non-creation phase",
            )),
        }
    }
}

/// One-shot guard for explicitly unfenced callers. It owns no durable marker
/// and therefore cannot be replayed or adopted as destructive authority — only
/// a [`crate::abandoned_realization::LegacyCreationRecord`], which says nothing
/// more than "an unfenced creation at this exact root is in flight" and is
/// published before the root exists so a cancelled attempt is still nameable.
pub(crate) struct LegacyCreationGuard {
    root: PathBuf,
    identity: Option<DirectoryIdentity>,
    lock: Option<awaken_sandbox_fs::ExclusiveFileLock>,
}

impl LegacyCreationGuard {
    fn lock(&self) -> Result<&awaken_sandbox_fs::ExclusiveFileLock, pc::SandboxError> {
        self.lock.as_ref().ok_or_else(|| {
            pc::SandboxError::conflict("legacy filesystem creation lock was released")
        })
    }

    pub(crate) fn validate_before_mutation(&self) -> Result<(), pc::SandboxError> {
        require_marker_absence_locked(&self.root, self.lock()?)?;
        let identity = self
            .identity
            .ok_or_else(|| pc::SandboxError::not_ready("legacy sandbox root was not prepared"))?;
        // The record this creation published must still name the exact root it
        // is about to mutate. Without this, a guard whose record was retained
        // or removed underneath would keep writing into a root that a later
        // attempt has already been told to refuse.
        let record = crate::abandoned_realization::read_creation_record(self.lock()?, &self.root)?
            .ok_or_else(|| {
                pc::SandboxError::gone("legacy sandbox creation lost its durable creation record")
            })?;
        if record.is_retained() || record.root_identity() != Some(identity) {
            return Err(pc::SandboxError::conflict(
                "legacy sandbox creation record no longer names the root being mutated",
            ));
        }
        require_exact_root_locked(&self.root, self.lock()?, identity)
    }

    pub(crate) fn prepare_root(&mut self) -> Result<(), pc::SandboxError> {
        require_marker_absence_locked(&self.root, self.lock()?)?;
        if let Some(identity) = self.identity {
            // Reclaiming an abandoned attempt: keep the inode the record names
            // and clear only what that attempt left inside it, exactly as the
            // fenced guard does for an incomplete Creating root.
            require_exact_root_locked(&self.root, self.lock()?, identity)?;
            return self
                .lock()?
                .clear_sibling_directory_contents_exact(root_leaf(&self.root)?, identity)
                .map_err(|error| pc::SandboxError::backend("prepare_root", error));
        }
        require_absent_locked(
            &self.root,
            self.lock()?,
            root_leaf(&self.root)?,
            "legacy sandbox root",
        )?;
        let identity = self
            .lock()?
            .create_sibling_directory_noreplace(root_leaf(&self.root)?)
            .map_err(|error| pc::SandboxError::backend("prepare_root", error))?;
        // Bind the record to the inode immediately. Until this lands, a later
        // attempt still recognizes the root — the record standing at all is
        // what makes it ours — but it cannot detect a substitution.
        crate::abandoned_realization::write_creation_record(
            self.lock()?,
            &self.root,
            &crate::abandoned_realization::LegacyCreationRecord::in_flight(Some(identity)),
        )?;
        self.identity = Some(identity);
        Ok(())
    }

    pub(crate) fn complete(&mut self) -> Result<LegacyLiveEvidence, pc::SandboxError> {
        let identity = self
            .identity
            .ok_or_else(|| pc::SandboxError::not_ready("legacy sandbox root was not prepared"))?;
        require_exact_root_locked(&self.root, self.lock()?, identity)?;
        // The creation settled, so the root is live work from here on. Dropping
        // the record is what makes a finished sandbox stop looking like an
        // abandoned one to the next attempt at this scope.
        crate::abandoned_realization::clear_creation_record(self.lock()?, &self.root)?;
        let realization = LegacyLiveEvidence {
            root_identity: identity,
        };
        self.lock.take();
        Ok(realization)
    }

    /// A deterministic failure whose compensation could not finish states that
    /// the root and its evidence were kept on purpose. That is the opposite of
    /// an abandonment — the owner is still speaking, and what it says is "do
    /// not reuse this" — so the record stops being reclaimable.
    pub(crate) fn retain_failed_compensation(&self) -> Result<(), pc::SandboxError> {
        crate::abandoned_realization::retain_creation_record(self.lock()?, &self.root)
    }

    pub(crate) fn abort_creation(self) -> Result<(), pc::SandboxError> {
        if let Some(identity) = self.identity {
            self.lock()?
                .remove_sibling_directory_tree_exact(root_leaf(&self.root)?, identity)
                .map_err(|error| pc::SandboxError::backend("abort_creation", error))?;
        }
        crate::abandoned_realization::clear_creation_record(self.lock()?, &self.root)
    }
}

mod creation_guard;
pub(crate) use creation_guard::ProviderCreationGuard;
mod removal_guard;

/// Serializes exact terminal cleanup and retains a Removed tombstone for
/// delete/finish/receipt response-loss replay.
/// Its terminal preparation/reconstruction/physical-removal methods validate
/// identity/provenance, not a second clock. Production callers enter the
/// existing current-effect guard before those synchronous mutations; Memory
/// acknowledgement already holds it, so re-entering authority here would
/// deadlock the original owner locks. Checkpoint WAL calls likewise enter
/// through the provider's borrowed CurrentCheckpointParticipant adapter.
pub(crate) struct RemovalGuard {
    root: PathBuf,
    marker: RealizationMarker,
    checkpoint_expected_effect_fence: Option<pc::SandboxEffectFence>,
    lock: awaken_sandbox_fs::ExclusiveFileLock,
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
mod execution_participant;
#[cfg(target_os = "linux")]
pub(crate) use execution_participant::{begin_execution, verify_execution_quiescence};

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
        pc::SandboxError::backend(
            "classify",
            format!(
                "inspect sandbox realization path `{}`: {error}",
                path.display()
            ),
        )
    })
}

fn classify_locked(
    lock: &awaken_sandbox_fs::ExclusiveFileLock,
    leaf: &Path,
) -> Result<PathEntry, pc::SandboxError> {
    lock.classify_sibling_nofollow(leaf).map_err(|error| {
        pc::SandboxError::backend(
            "classify_locked",
            format!(
                "inspect locked sandbox realization sibling `{}`: {error}",
                leaf.display()
            ),
        )
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
        _ => Err(pc::SandboxError::conflict(format!(
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
        .ok_or_else(|| pc::SandboxError::invalid("sandbox root has no portable final component"))?;
    let prefix = format!(".{name}.awaken-realization-stage-");
    if lock
        .sibling_names()
        .map_err(|error| pc::SandboxError::backend("require_no_untracked_stage_locked", error))?
        .iter()
        .any(|candidate| candidate.to_string_lossy().starts_with(&prefix))
    {
        Err(pc::SandboxError::conflict(
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
        PathEntry::Absent => Err(pc::SandboxError::not_found(format!(
            "sandbox root `{}` is absent",
            root.display()
        ))),
        _ => Err(pc::SandboxError::conflict(format!(
            "sandbox root `{}` has a foreign file type or identity",
            root.display()
        ))),
    }
}

fn decode_marker(bytes: &[u8], display: &Path) -> Result<RealizationMarker, pc::SandboxError> {
    let marker: RealizationMarker = serde_json::from_slice(bytes).map_err(|error| {
        pc::SandboxError::invalid(format!(
            "decode sandbox realization marker `{}`: {error}",
            display.display()
        ))
    })?;
    if marker.schema != MARKER_SCHEMA {
        return Err(pc::SandboxError::unsupported(format!(
            "sandbox realization marker `{}` has unsupported schema {}",
            display.display(),
            marker.schema
        )));
    }
    if marker.physical_incarnation.trim().is_empty() {
        return Err(pc::SandboxError::invalid(
            "sandbox realization marker has an empty physical incarnation",
        ));
    }
    validate_stage_leaf(&marker.root)?;
    execution_participant::validate(&marker)?;
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
        return Err(pc::SandboxError::invalid(
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
        return Err(pc::SandboxError::conflict(
            "filesystem disposal authorization conflicts with the marker effect",
        ));
    }
    Ok(())
}

fn read_marker_path(path: &Path) -> Result<RealizationMarker, pc::SandboxError> {
    let bytes = awaken_sandbox_fs::read_regular_file_nofollow(path).map_err(|error| {
        pc::SandboxError::backend(
            "read_marker_path",
            format!(
                "read sandbox realization marker `{}`: {error}",
                path.display()
            ),
        )
    })?;
    decode_marker(&bytes, path)
}

fn read_marker(root: &Path) -> Result<Option<RealizationMarker>, pc::SandboxError> {
    let path = marker_path(root)?;
    match classify(&path)? {
        PathEntry::Absent => Ok(None),
        PathEntry::RegularFile => read_marker_path(&path).map(Some),
        _ => Err(pc::SandboxError::invalid(format!(
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
                    pc::SandboxError::backend(
                        "read_marker_locked",
                        format!(
                            "read locked sandbox realization marker `{}`: {error}",
                            marker_path(root).map_or_else(
                                |_| leaf.display().to_string(),
                                |path| path.display().to_string()
                            )
                        ),
                    )
                })?;
            decode_marker(&bytes, &marker_path(root)?).map(Some)
        }
        _ => Err(pc::SandboxError::invalid(format!(
            "sandbox realization marker `{}` is not a regular file",
            marker_path(root)?.display()
        ))),
    }
}

fn encode_marker(marker: &RealizationMarker) -> Result<Vec<u8>, pc::SandboxError> {
    serde_json::to_vec(marker).map_err(|error| pc::SandboxError::backend("encode_marker", error))
}

fn publish_marker_locked(
    root: &Path,
    lock: &awaken_sandbox_fs::ExclusiveFileLock,
    marker: &RealizationMarker,
) -> Result<(), pc::SandboxError> {
    lock.publish_sibling_file_noreplace(&marker_leaf(root)?, &encode_marker(marker)?)
        .map_err(|error| pc::SandboxError::backend("publish_marker_locked", error))
}

fn replace_marker_locked(
    root: &Path,
    lock: &awaken_sandbox_fs::ExclusiveFileLock,
    expected: &RealizationMarker,
    next: &RealizationMarker,
) -> Result<(), pc::SandboxError> {
    let leaf = marker_leaf(root)?;
    if read_marker_locked(root, lock)? != Some(expected.clone()) {
        return Err(pc::SandboxError::conflict(
            "sandbox realization marker changed during a locked transition",
        ));
    }
    lock.replace_sibling_regular_file_atomic(&leaf, &encode_marker(next)?)
        .map_err(|error| pc::SandboxError::backend("replace_marker_locked", error))
}

fn remove_marker_exact_locked(
    root: &Path,
    lock: &awaken_sandbox_fs::ExclusiveFileLock,
    expected: &RealizationMarker,
) -> Result<(), pc::SandboxError> {
    let leaf = marker_leaf(root)?;
    if read_marker_locked(root, lock)? != Some(expected.clone()) {
        return Err(pc::SandboxError::conflict(
            "sandbox realization marker changed before compensation",
        ));
    }
    lock.remove_sibling_regular_file_nofollow(&leaf)
        .map_err(|error| pc::SandboxError::backend("remove_marker_exact_locked", error))
}

fn require_marker_absence_locked(
    root: &Path,
    lock: &awaken_sandbox_fs::ExclusiveFileLock,
) -> Result<(), pc::SandboxError> {
    match read_marker_locked(root, lock)? {
        None => Ok(()),
        Some(_) => Err(pc::SandboxError::unsupported(format!(
            "legacy sandbox handle cannot claim current realization evidence `{}`",
            marker_path(root)?.display()
        ))),
    }
}

fn required_root_identity(
    marker: &RealizationMarker,
) -> Result<DirectoryIdentity, pc::SandboxError> {
    marker.root.identity.map(Into::into).ok_or_else(|| {
        pc::SandboxError::not_found("sandbox realization marker has no root inode evidence")
    })
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
                return Err(pc::SandboxError::conflict(format!(
                    "sandbox private stage `{}` exists without durable inode evidence",
                    stage.display()
                )));
            }
        }
        let previous = marker.clone();
        marker.root.identity = Some(
            lock.create_sibling_directory_noreplace(&stage_leaf)
                .map_err(|error| pc::SandboxError::backend("ensure_root_published", error))?
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
            lock.publish_sibling_directory_noreplace(&stage_leaf, root_leaf)
                .map_err(|error| pc::SandboxError::backend("ensure_root_published", error))?;
            let previous = marker.clone();
            marker.root.published = true;
            replace_marker_locked(root, lock, &previous, marker)
        }
        // Rename succeeded and the process died before publishing the phase bit.
        (false, PathEntry::Directory(observed), PathEntry::Absent) if observed == identity => {
            let previous = marker.clone();
            marker.root.published = true;
            replace_marker_locked(root, lock, &previous, marker)
        }
        (true, PathEntry::Directory(observed), PathEntry::Absent) if observed == identity => Ok(()),
        _ => Err(pc::SandboxError::conflict(format!(
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
            _ => Err(pc::SandboxError::invalid(
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
        _ => Err(pc::SandboxError::conflict(format!(
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
    execution_participant::require_quiescent(marker)?;
    match owned_realization_path_locked(root, lock, marker)? {
        OwnedRealizationPath::Absent => Ok(()),
        OwnedRealizationPath::Exact { path, .. } => Err(pc::SandboxError::conflict(format!(
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
    execution_participant::require_quiescent(marker)?;
    match owned_realization_path_locked(root, lock, marker)? {
        OwnedRealizationPath::Absent => Ok(()),
        OwnedRealizationPath::Exact { path, identity } => {
            let leaf = if path == root {
                root_leaf(root)?
            } else {
                stage_leaf(&marker.root)?
            };
            lock.remove_sibling_directory_tree_exact(leaf, identity)
                .map_err(|error| {
                    pc::SandboxError::backend("remove_owned_root_or_stage_locked", error)
                })
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
        executions: Vec::new(),
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
        if source.is_some() {
            return Err(pc::SandboxError::not_found(
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
        return Err(pc::SandboxError::conflict(
            "sandbox realization belongs to a different immutable specification",
        ));
    }
    execution_participant::require_quiescent(&observed)?;
    match observed.phase {
        RealizationPhase::Creating => {
            if source.is_some()
                || !observed.effect_fence.same_effect_identity(effect_fence)
                || observed.restore_input_fingerprint.as_deref() != restore_input_fingerprint
            {
                return Err(pc::SandboxError::conflict(
                    "incomplete sandbox creation belongs to another effect",
                ));
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
            let recorded = observed.rebuild_source.as_ref().ok_or_else(|| {
                pc::SandboxError::not_found("recreating sandbox has no source evidence")
            })?;
            if recorded.phase == RealizationPhase::Removed {
                let restore_input = restore_input_fingerprint.ok_or_else(|| {
                    pc::SandboxError::invalid(
                        "checkpoint restore retry has no exact restore input fingerprint",
                    )
                })?;
                if source.is_some()
                    || !observed.effect_fence.same_effect_identity(effect_fence)
                    || observed.restore_input_fingerprint.as_deref() != Some(restore_input)
                {
                    return Err(pc::SandboxError::conflict(
                        "checkpoint restore retry conflicts with its Removed predecessor",
                    ));
                }
                validate_removed_restore_source(recorded, restore_input)?;
            } else {
                let source = source.ok_or_else(|| {
                    pc::SandboxError::invalid("recreating sandbox requires its exact source handle")
                })?;
                if recorded.physical_incarnation != source.physical_incarnation
                    || !recorded
                        .effect_fence
                        .same_effect_identity(source.effect_fence)
                    || !observed.effect_fence.same_effect_identity(effect_fence)
                    || observed.fingerprint != *source.fingerprint
                    || observed.restore_input_fingerprint.as_deref() != restore_input_fingerprint
                {
                    return Err(pc::SandboxError::conflict(
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
                    Err(pc::SandboxError::conflict(
                        "ready sandbox root conflicts with another create or rebuild effect",
                    ))
                }
            }
            PathEntry::Absent => {
                let source = source.ok_or_else(|| {
                    pc::SandboxError::not_found(
                        "missing ready sandbox requires an exact V2 rebuild source handle",
                    )
                })?;
                if !source_matches(&observed, &source)
                    || !is_distinct_authorized_rebuild(&observed.effect_fence, effect_fence)
                {
                    return Err(pc::SandboxError::fence_rejected(
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
                    executions: observed.executions.clone(),
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
            _ => Err(pc::SandboxError::conflict(
                "ready sandbox root has a foreign type or physical identity",
            )),
        },
        RealizationPhase::Removed => {
            if source.is_some() {
                return Err(pc::SandboxError::fence_rejected(
                    "Removed filesystem restoration does not accept a stale source handle",
                ));
            }
            let restore_input = restore_input_fingerprint.ok_or_else(|| {
                pc::SandboxError::invalid(
                    "Removed filesystem can only transition through an exact checkpoint restore",
                )
            })?;
            if !is_distinct_authorized_rebuild(&observed.effect_fence, effect_fence) {
                return Err(pc::SandboxError::fence_rejected(
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
                executions: observed.executions.clone(),
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
        RealizationPhase::Removing => Err(pc::SandboxError::gone(
            "terminal sandbox realization cannot be created or rebuilt",
        )),
    }
}

/// Admit one unfenced creation at `root`, reclaiming this authority's own
/// abandoned attempt there and refusing everything else.
///
/// The record is published before the guard is returned, and the guard is the
/// only way to reach [`LegacyCreationGuard::prepare_root`] — the first
/// filesystem mutation — so no caller can create a root before the statement
/// that names it exists. Admission itself is
/// [`crate::abandoned_realization::classify_legacy_root`], which owns the
/// reclaimable/refused rule as one table.
pub(crate) fn begin_legacy(root: &Path) -> Result<LegacyCreationGuard, pc::SandboxError> {
    use crate::abandoned_realization::{LegacyCreationRecord, LegacyRootDisposition};

    let lock = acquire(root)?;
    require_marker_absence_locked(root, &lock)?;
    let record = crate::abandoned_realization::read_creation_record(&lock, root)?;
    let identity = match crate::abandoned_realization::classify_legacy_root(
        record.as_ref(),
        classify_locked(&lock, root_leaf(root)?)?,
    ) {
        LegacyRootDisposition::Create => None,
        LegacyRootDisposition::Reclaim(identity) => Some(identity),
        LegacyRootDisposition::Refuse(reason) => {
            return Err(pc::SandboxError::invalid(format!(
                "legacy sandbox root `{}` {reason}",
                root.display()
            )));
        }
    };
    crate::abandoned_realization::write_creation_record(
        &lock,
        root,
        &LegacyCreationRecord::in_flight(identity),
    )?;
    Ok(LegacyCreationGuard {
        root: root.to_owned(),
        identity,
        lock: Some(lock),
    })
}

mod observation;
pub(crate) use observation::{
    TerminalReceiptObservation, observe_adoption, observe_terminal_receipt, verify_adoption,
};

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
        return Err(pc::SandboxError::conflict(
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
    let guard =
        begin_ready_operation_with_authorization(root, evidence, effect_fence, effect_fence)?;
    execution_participant::require_quiescent(&guard.marker)?;
    Ok(guard)
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
        return Err(pc::SandboxError::invalid(
            "terminal checkpoint cleanup requires distinct realization, Suspend, and terminal effects",
        ));
    }
    let guard = begin_ready_operation_with_authorization(
        root,
        evidence,
        expected_effect_fence,
        terminal_effect_fence,
    )?;
    execution_participant::require_quiescent(&guard.marker)?;
    Ok(guard)
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
    let marker = read_marker_locked(root, &lock)?.ok_or_else(|| {
        pc::SandboxError::not_found("ready filesystem operation has no realization marker")
    })?;
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
/// and the same lock observed or durably published the `Removed` tombstone.
pub(crate) fn begin_terminal_takeover(
    root: &Path,
    fingerprint: &pc::SandboxRealizationFingerprint,
    handle: Option<RebuildSource<'_>>,
    expected_effect_fence: Option<&pc::SandboxEffectFence>,
    terminal_effect_fence: &pc::SandboxEffectFence,
    expected_preflight: Option<&TerminalReceiptObservation>,
) -> Result<Option<(RealizationEvidence, RemovalGuard)>, pc::SandboxError> {
    // Both production entrances hold current Worker/Session authority across
    // this synchronous function. Control expiry is an identity/order coordinate,
    // never a value to compare with this machine's UTC.
    terminal_effect_fence.validate_identity()?;
    if let Some(expected) = expected_effect_fence {
        expected.validate_identity()?;
    }
    let lock = acquire(root)?;
    let observed = read_marker_locked(root, &lock)?;
    observation::validate_terminal_preflight(expected_preflight, observed.as_ref())?;
    let Some(mut marker) = observed else {
        let (terminal_source, incarnation) = observation::validate_absent_terminal_source(
            fingerprint,
            handle.as_ref(),
            expected_effect_fence,
            terminal_effect_fence,
        )?;
        require_absent_locked(root, &lock, root_leaf(root)?, "terminal sandbox root")?;
        if let Some(incarnation) = incarnation.as_ref() {
            let record = root_record(root, incarnation)?;
            require_absent_locked(
                root,
                &lock,
                stage_leaf(&record)?,
                "terminal sandbox private stage",
            )?;
        } else {
            require_no_untracked_stage_locked(root, &lock)?;
        }
        // A marker-free terminal response cannot simply return absence: an old
        // but still-live create effect could otherwise publish after this lock
        // is released. Persist the existing Removed phase as the one durable
        // lifecycle fence; ordinary creation already rejects this tombstone and
        // only an exact completed-checkpoint restore may ever leave it.
        let mut tombstone = marker_for_new_attempt(
            root,
            fingerprint,
            terminal_effect_fence,
            RealizationPhase::Removed,
            None,
            None,
        )?;
        if let Some(incarnation) = incarnation.as_ref() {
            tombstone.physical_incarnation = incarnation.clone();
            tombstone.root = root_record(root, incarnation)?;
        }
        tombstone.terminal_source = Some(terminal_source);
        // Publication remains inside the same current-authority callback and
        // original nonblocking lifecycle lock as observation.
        publish_marker_locked(root, &lock, &tombstone)?;
        return Ok(None);
    };

    let terminal_source = observation::validate_terminal_source(
        &marker,
        fingerprint,
        handle.as_ref(),
        expected_effect_fence,
        terminal_effect_fence,
    )?;

    let owned = owned_realization_path_locked(root, &lock, &marker)?;
    if marker.phase == RealizationPhase::Removed {
        execution_participant::require_quiescent(&marker)?;
        return match owned {
            OwnedRealizationPath::Absent => Ok(None),
            OwnedRealizationPath::Exact { .. } => Err(pc::SandboxError::conflict(
                "removed filesystem realization still has a physical participant",
            )),
        };
    }
    if marker.phase == RealizationPhase::Ready
        && matches!(&owned, OwnedRealizationPath::Exact { path, .. } if path != root)
    {
        return Err(pc::SandboxError::conflict(
            "ready filesystem realization is stranded at its private stage",
        ));
    }

    if marker.phase != RealizationPhase::Removing {
        // Source/receipt/root preflight is deliberately effect-free. The held
        // current-authority callback also spans this first durable phase write.
        let previous = marker.clone();
        marker.phase = RealizationPhase::Removing;
        marker.effect_fence = terminal_effect_fence.clone();
        marker.terminal_source = Some(terminal_source.clone());
        replace_marker_locked(root, &lock, &previous, &marker)?;
    } else if !marker
        .effect_fence
        .authorizes_successor(terminal_effect_fence)
    {
        return Err(pc::SandboxError::fence_rejected(
            "terminal cleanup fence is stale or foreign",
        ));
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
            execution_participant::require_quiescent(&marker)?;
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
        return Err(pc::SandboxError::unsupported(
            "legacy adopted sandbox has no destructive root evidence",
        ));
    };
    let lock = acquire(root)?;
    require_marker_absence_locked(root, &lock)?;
    match classify_locked(&lock, root_leaf(root)?)? {
        PathEntry::Absent => Ok(()),
        PathEntry::Directory(observed) if observed == identity => lock
            .remove_sibling_directory_tree_exact(root_leaf(root)?, identity)
            .map_err(|error| pc::SandboxError::backend("dispose_legacy", error)),
        _ => Err(pc::SandboxError::conflict(
            "legacy sandbox root was substituted before disposal",
        )),
    }
}

#[cfg(test)]
#[path = "realization_marker/tests.rs"]
mod tests;
