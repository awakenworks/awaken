//! Where a cancelled provider creation leaves the name of the root it touched.
//!
//! The durable record for a filesystem-tier realization is the on-disk marker
//! that [`crate::realization_marker::begin`] writes before the first mutation,
//! and `observe_adoption` is the scan that reads it back. That covers the
//! effect itself. What it cannot cover is the window *before* it: a caller
//! dropped between the claim and the marker write leaves either nothing at all
//! or a half-prepared root, and the two are indistinguishable without walking
//! the whole provider base.
//!
//! So the claim's retention names the exact root. The ledger turns "scan every
//! sandbox under `base` and decide" into "look at this one", which is the
//! difference between a reconciliation an operator runs and one they don't.
//! It is deliberately in-process: it is an index into the durable evidence,
//! never a substitute for it.
//!
//! # The unfenced path has no such durable evidence to index
//!
//! [`crate::realization_marker::begin_legacy`] serves the explicitly unfenced
//! callers — the ones an Agent Run reaches through `create_sandbox` — and it
//! writes no realization marker by design: a marker-free handle must never be
//! adoptable as destructive authority. It used to write *nothing at all*, and
//! that made a cancelled legacy creation worse than a leak. The old admission
//! rule was "the root must be absent"; a caller dropped after the root was
//! published left a bare directory and no statement about it, so every later
//! attempt at that scope failed closed forever. A scope that can never be
//! retried is a wedge, and no operator scan can undo it because nothing on disk
//! says the directory was an unfinished creation rather than live work.
//!
//! [`LegacyCreationRecord`] is the missing statement, and it is deliberately
//! the weakest thing that closes the wedge: a sibling note saying "an unfenced
//! creation at this root is in flight", published under the same parent lock
//! *before* the root is created and removed when the creation settles. It
//! carries no fence, no fingerprint and no incarnation, so it cannot be
//! mistaken for realization evidence or grant anyone destructive authority
//! beyond the one root it names.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use awaken_provisioning_contract as pc;
use awaken_sandbox_fs::{DirectoryIdentity, PathEntry};
use serde::{Deserialize, Serialize};

/// Roots whose realization was claimed and then dropped before it settled.
#[derive(Default)]
pub struct AbandonedRealizations {
    roots: Mutex<Vec<PathBuf>>,
}

impl AbandonedRealizations {
    /// The retention handed to [`pc::EffectClaim::durable`](awaken_provisioning_contract::EffectClaim::durable):
    /// it records `root` if and only if the claimed creation is dropped
    /// unsettled. Built before the effect, because a retention constructed
    /// afterwards would be exactly the ordering this whole mechanism exists to
    /// forbid.
    pub(crate) fn retain(self: &Arc<Self>, root: PathBuf) -> impl FnOnce() + Send + 'static {
        let ledger = Arc::clone(self);
        move || {
            ledger
                .roots
                .lock()
                .expect("abandoned realization ledger poisoned")
                .push(root);
        }
    }

    /// Every root recorded so far, oldest first.
    #[must_use]
    pub fn roots(&self) -> Vec<PathBuf> {
        self.roots
            .lock()
            .expect("abandoned realization ledger poisoned")
            .clone()
    }

    /// Whether `root` is among them.
    #[must_use]
    pub fn contains(&self, root: &Path) -> bool {
        self.roots
            .lock()
            .expect("abandoned realization ledger poisoned")
            .iter()
            .any(|recorded| recorded == root)
    }
}

/// The scan a filesystem-tier claim names: the reconciler that reads a
/// provider realization marker back and decides the stranded root's fate.
/// Named once here so the two providers cannot disagree about who reconciles
/// them, and so the name stays a real symbol rather than a string.
pub(crate) const REALIZATION_RECONCILER: &str = "observe_adoption";

const LEGACY_CREATION_SCHEMA: u8 = 1;

/// The durable note one unfenced creation leaves beside the root it is about
/// to make. Its whole job is to separate two directories that look identical
/// on disk: one this authority was in the middle of creating, and one that is
/// either live or somebody else's.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LegacyCreationRecord {
    schema: u8,
    /// The inode of the root this creation published, once it exists. `None`
    /// means the creation was interrupted at or before the directory syscall,
    /// so there is nothing yet to bind the record to.
    #[serde(default)]
    root: Option<RootInode>,
    /// Set when a deterministic failure's compensation could not finish. See
    /// [`LegacyRootDisposition`] for why that is the opposite of abandonment.
    #[serde(default)]
    retained: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RootInode {
    device: u64,
    inode: u64,
}

impl LegacyCreationRecord {
    /// A creation that is in flight and owes an outcome, optionally already
    /// bound to the root inode it published.
    pub(crate) fn in_flight(root: Option<DirectoryIdentity>) -> Self {
        Self {
            schema: LEGACY_CREATION_SCHEMA,
            root: root.map(|identity| RootInode {
                device: identity.device,
                inode: identity.inode,
            }),
            retained: false,
        }
    }

    pub(crate) fn root_identity(&self) -> Option<DirectoryIdentity> {
        self.root.map(|root| DirectoryIdentity {
            device: root.device,
            inode: root.inode,
        })
    }

    pub(crate) fn is_retained(&self) -> bool {
        self.retained
    }

    fn retained(&self) -> Self {
        Self {
            retained: true,
            ..self.clone()
        }
    }
}

/// What [`crate::realization_marker::begin_legacy`] may do with the root it
/// finds. Total over every (record, observed entry) pair, so the admission rule
/// is one table instead of a chain of guards spread across two call sites.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LegacyRootDisposition {
    /// Nothing stands here and nothing claims to: make the root.
    Create,
    /// This authority's own unsettled creation published this exact root.
    /// Clearing its contents and reusing the same inode is a *retry*, not a
    /// takeover: the inode stays stable, so the record that names it stays
    /// true and no same-scope ABA can slip between the two attempts.
    Reclaim(DirectoryIdentity),
    /// Refuse, with the reason the caller appends to the root's name.
    Refuse(&'static str),
}

/// The one rule that distinguishes a reclaimable abandonment from a root that
/// must still be refused.
///
/// **Reclaimable** is narrow on purpose: an in-flight record written by this
/// provider must still stand, and the directory it finds must be the one that
/// record names (or a directory that appeared while the record stood, which
/// under the parent's exclusive lock can only be that creation's own). Nothing
/// else qualifies.
///
/// **Refused** covers the three ways a directory can be someone else's
/// business. A root with *no* record is either live — `complete` deletes the
/// record precisely so a finished sandbox stops looking like an abandonment —
/// or foreign, and the two are indistinguishable, so both fail closed. A root
/// whose inode differs from the recorded one was substituted underneath. And a
/// `retained` record is an explicit statement, not silence: a deterministic
/// failure whose compensation could not finish *chose* to keep the root and its
/// evidence, so reusing it would destroy exactly what it preserved. Silence is
/// recoverable; a statement to the contrary is not.
///
/// Causes: C1 the record (absent / in-flight / retained); C2 the observed entry
/// (absent / directory / other file type); C3 the recorded inode against the
/// observed one (unbound / equal / different).
/// Effects: E1 create, E2 reclaim, E3 refuse.
///
/// | Rule | C1        | C2        | C3        | Effect |
/// | R1   | absent    | absent    | n/a       | E1     |
/// | R2   | absent    | any other | n/a       | E3     |
/// | R3   | in-flight | absent    | n/a       | E1     |
/// | R4   | in-flight | directory | unbound   | E2     |
/// | R5   | in-flight | directory | equal     | E2     |
/// | R6   | in-flight | directory | different | E3     |
/// | R7   | in-flight | other     | n/a       | E3     |
/// | R8   | retained  | any       | any       | E3     |
pub(crate) fn classify_legacy_root(
    record: Option<&LegacyCreationRecord>,
    observed: PathEntry,
) -> LegacyRootDisposition {
    match record {
        // R8 — an explicit retention outranks every observation.
        Some(record) if record.is_retained() => LegacyRootDisposition::Refuse(
            "was retained by a failed compensation and cannot be reused",
        ),
        // R1/R2 — no creation is on record, so only absence is admissible.
        None => match observed {
            PathEntry::Absent => LegacyRootDisposition::Create,
            _ => LegacyRootDisposition::Refuse("is not absent and no creation of ours claims it"),
        },
        Some(record) => match (observed, record.root_identity()) {
            // R3 — the creation never got as far as the directory.
            (PathEntry::Absent, _) => LegacyRootDisposition::Create,
            // R4 — the directory appeared while this record stood, and the
            // parent lock admits one creation at a time, so it is ours.
            (PathEntry::Directory(found), None) => LegacyRootDisposition::Reclaim(found),
            // R5/R6 — the record names an inode; only that exact one is ours.
            (PathEntry::Directory(found), Some(recorded)) if found == recorded => {
                LegacyRootDisposition::Reclaim(found)
            }
            (PathEntry::Directory(_), Some(_)) => {
                LegacyRootDisposition::Refuse("was substituted under an unfinished creation")
            }
            // R7 — a file or symlink is never a sandbox root of ours.
            _ => LegacyRootDisposition::Refuse("has a foreign file type"),
        },
    }
}

/// The record's sibling leaf. It shares the root's parent (and therefore the
/// parent lock every legacy transition already holds) and is named apart from
/// both the realization marker and the private stage prefix, so no existing
/// scan mistakes it for either.
fn creation_record_leaf(root: &Path) -> Result<PathBuf, pc::SandboxError> {
    let name = root
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| error("sandbox root has no portable final component"))?;
    Ok(PathBuf::from(format!(
        ".{name}.awaken-legacy-creation.json"
    )))
}

pub(crate) fn read_creation_record(
    lock: &awaken_sandbox_fs::ExclusiveFileLock,
    root: &Path,
) -> Result<Option<LegacyCreationRecord>, pc::SandboxError> {
    let leaf = creation_record_leaf(root)?;
    match lock.classify_sibling_nofollow(&leaf).map_err(error)? {
        PathEntry::Absent => Ok(None),
        PathEntry::RegularFile => {
            let bytes = lock
                .read_sibling_regular_file_nofollow(&leaf)
                .map_err(error)?;
            let record: LegacyCreationRecord = serde_json::from_slice(&bytes).map_err(|cause| {
                error(format!(
                    "decode legacy sandbox creation record `{}`: {cause}",
                    leaf.display()
                ))
            })?;
            if record.schema != LEGACY_CREATION_SCHEMA {
                return Err(error(format!(
                    "legacy sandbox creation record `{}` has unsupported schema {}",
                    leaf.display(),
                    record.schema
                )));
            }
            Ok(Some(record))
        }
        _ => Err(error(format!(
            "legacy sandbox creation record `{}` is not a regular file",
            leaf.display()
        ))),
    }
}

pub(crate) fn write_creation_record(
    lock: &awaken_sandbox_fs::ExclusiveFileLock,
    root: &Path,
    record: &LegacyCreationRecord,
) -> Result<(), pc::SandboxError> {
    let leaf = creation_record_leaf(root)?;
    let bytes = serde_json::to_vec(record).map_err(error)?;
    // Both edges go through the same no-follow inode checks the realization
    // marker uses; only the first write has nothing to replace.
    match lock.classify_sibling_nofollow(&leaf).map_err(error)? {
        PathEntry::Absent => lock
            .publish_sibling_file_noreplace(&leaf, &bytes)
            .map_err(error),
        PathEntry::RegularFile => lock
            .replace_sibling_regular_file_atomic(&leaf, &bytes)
            .map_err(error),
        _ => Err(error(format!(
            "legacy sandbox creation record `{}` is not a regular file",
            leaf.display()
        ))),
    }
}

/// Mark the record so no later attempt may reclaim the root it names. If even
/// that write fails, remove the record instead: a root with no record is
/// refused just as hard, and leaving a reclaimable one behind is the only
/// outcome that would be unsafe.
pub(crate) fn retain_creation_record(
    lock: &awaken_sandbox_fs::ExclusiveFileLock,
    root: &Path,
) -> Result<(), pc::SandboxError> {
    let Some(record) = read_creation_record(lock, root)? else {
        return Ok(());
    };
    match write_creation_record(lock, root, &record.retained()) {
        Ok(()) => Ok(()),
        Err(cause) => clear_creation_record(lock, root).map_err(|removal| {
            error(format!(
                "retain legacy sandbox creation record: {cause}; removal also failed: {removal}"
            ))
        }),
    }
}

pub(crate) fn clear_creation_record(
    lock: &awaken_sandbox_fs::ExclusiveFileLock,
    root: &Path,
) -> Result<(), pc::SandboxError> {
    let leaf = creation_record_leaf(root)?;
    match lock.classify_sibling_nofollow(&leaf).map_err(error)? {
        PathEntry::Absent => Ok(()),
        _ => lock
            .remove_sibling_regular_file_nofollow(&leaf)
            .map_err(error),
    }
}

fn error(cause: impl ToString) -> pc::SandboxError {
    pc::SandboxError::new(cause.to_string())
}

#[cfg(test)]
#[path = "abandoned_realization/tests.rs"]
mod tests;
