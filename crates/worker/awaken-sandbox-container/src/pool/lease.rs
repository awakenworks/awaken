// @formal surface obligation=lease.timing_has_three_renewal_windows
// @formal surface obligation=lease.recovery_grace_covers_reconciliation
// @formal surface obligation=lease.loss_only_fences
// @formal surface obligation=claimed_effect.retention_runs_once_for_an_unsettled_drop
// @formal reason: Static owner: the host lease record over warm container capacity - one file per warm
// @formal reason: scope whose mtime is the deadline, minted (linked) before the warm create's
// @formal reason: ClaimedEffect exists, renewed in place on the existing inode, consumed either by Session
// @formal reason: checkout (unlink) or by a reclaimer claim (rename), never both. Dynamic path: timing is
// @formal reason: validated by the shared LeaseTimingPolicy (three renewal windows, grace covers one
// @formal reason: reclamation interval); a reclaimer removes a scope only after an exclusive claim on an
// @formal reason: expired record whose post-rename deadline re-read still shows expiry, so a renewing
// @formal reason: owner or a checked-out Session container is never reached; lease loss makes the owner
// @formal reason: forget, never remove. The rename/unlink atomicity and the removal itself are real
// @formal reason: filesystem and daemon effects covered by pool::lease_tests (manual clock, fake daemon
// @formal reason: shared by several pools) and the docker/podman restart suites, not by a direct proof.
//! Leases over warm container capacity, so a restarted Worker can reclaim what
//! a dead incarnation left running.
//!
//! # Why a lease, and why it lives on the host filesystem
//!
//! Warm capacity is created unfenced: no Session receipt names it, so the
//! fenced disposal path can never reach it, and the in-process
//! `unreceipted_scopes` record dies with the process. What a successor needs is
//! a record that (a) outlives the process, (b) can be *enumerated* without
//! knowing any name in advance, (c) can be renewed while the owner is alive,
//! and (d) can be taken away atomically, so that exactly one party — a
//! reclaimer, or the owner checking the container out to a Session — wins.
//!
//! Container labels satisfy (a) and (b) but not (c): Docker and Podman labels
//! are immutable after create. So the record is a file, one per warm scope, in
//! a per-user, per-lease-domain directory on the Worker host. That is the same
//! trust this crate already places in the host for cross-process state: the
//! retained host staging (`awaken-acp-stage-*` under the temp dir) is what a
//! replacement Worker reopens after its creator died, and the local tier's
//! `LegacyCreationRecord` is a sibling note on the same filesystem. The lease
//! directory defaults to the same temp dir ([`WarmCapacityLeases::host_default`]).
//!
//! * **Durable**: the lease file. Its *modification time* is the lease deadline
//!   (an [`Expiry`] minted from the injected [`WallClock`], judged by the one
//!   expiry law: live while `now < deadline`); its content
//!   names scope, holder incarnation and lease domain for attribution and is
//!   never rewritten. The mtime is used because it is the one attribute that can
//!   be changed on an *existing inode* without creating anything: renewal can
//!   therefore never resurrect a record a reclaimer has already taken.
//! * **TCB**: the directory and everything that can write it — the Worker's own
//!   uid (the per-domain directory is created `0700` and refused unless it is
//!   owned by the current user and closed to group/other; its root must be owned
//!   by the same user and not world-writable) and the host kernel's `rename(2)`
//!   / `unlink(2)` atomicity within one directory. Anything that can write the
//!   directory can already reach the container runtime socket.
//! * **Not durable across a host reboot that clears the temp dir**, exactly like
//!   the retained host staging next to it.
//!
//! # The protocol
//!
//! | Party     | Operation | Filesystem primitive                                   |
//! |-----------|-----------|--------------------------------------------------------|
//! | owner     | mint      | write `<scope>.mint`, set deadline, `link` → `<scope>.lease` |
//! | owner     | renew     | open existing `<scope>.lease` (never create), set mtime, confirm the path still names that inode |
//! | owner     | check out | `unlink <scope>.lease`                                  |
//! | owner     | dispose   | `rename <scope>.lease → <scope>.claim.<owner>`, remove, `unlink` the claim |
//! | reclaimer | claim     | `rename <record> → <scope>.claim.<reclaimer>`, then re-read the deadline |
//! | reclaimer | finish    | remove the container by scope, then `unlink` the claim  |
//!
//! Check-out, the owner's disposal and a reclaimer's claim are the three
//! operations that consume the lease path, and each is one atomic syscall on
//! that name, so exactly one of them can succeed. That is the whole argument
//! for Rule L3, and the reason an owner and a reclaimer can never both remove
//! one container.
//!
//! # What a reclaimer may delete, and why that is safe
//!
//! A reclaimer removes only the container named by the scope of a record it
//! holds a successful claim on, through the unreceipted-capacity port
//! (`remove_unreceipted_scope`, which treats absence as success). Holding the
//! claim proves, at the moment of the rename:
//!
//! 1. the record was still at its lease path, so the container had not been
//!    checked out (check-out unlinks that path first — L3);
//! 2. the deadline had passed and no renewal was visible on the inode after the
//!    rename (the post-claim re-read puts back a record whose owner renewed in
//!    the window — L1);
//! 3. the scope is a warm scope minted by some pool in this lease domain, and
//!    the domain binds backend and realization namespace, so the removal
//!    addresses the same daemon-global name the creator used.
//!
//! The owner, on the other side, only ever hands a container to a Session after
//! its own unlink succeeded, and treats a renewal or check-out that finds its
//! record gone as "lost": it forgets that container without removing it,
//! because the claimant owns the removal. Lease loss therefore never makes two
//! parties remove the same container, and a claimant never removes a Session's.
//!
//! # Decision table
//!
//! | Rule | Cause                                                                | Effect |
//! |------|----------------------------------------------------------------------|--------|
//! | L1   | record deadline > now, or a renewal lands before the claim re-read    | not reclaimed; a record is not even briefly taken, and a claim the owner raced is put back unchanged and reported in `raced` |
//! | L2   | record deadline ≤ now (owner dead, or stalled past the TTL)           | claimed by exactly one reclaimer, container removed once, claim deleted; a losing reclaimer removes nothing |
//! | L2c  | a claim whose claimer died (claim deadline ≤ now)                     | re-claimed and finished by the next reclaimer |
//! | L3   | the container was checked out to a Session                           | its record was unlinked before hand-out, so no reclaimer can reach it; a candidate whose record was already claimed is never handed out |
//! | L4   | a warm create starts                                                 | the record is linked before the create future exists; a failed mint creates nothing |
//! | L5   | the pool is alive                                                    | every held record is renewed at an interval ≤ TTL/3; disposal claims the pool's own record first and deletes it only once the runtime confirmed removal, so a failed disposal leaves a claim that expires into reclamation |
//! | L6   | a record whose container no longer exists                            | reclamation treats absence as success and deletes it; a live pool drops a ready container that fails its liveness probe |
//! | L7   | production composition (Docker/Podman)                               | the pool reclaims at startup and runs renewal + periodic reclamation |
//!
//! # Not covered
//!
//! * A runtime without a lease domain (Kubernetes today) keeps the old
//!   behaviour: no record, no cross-restart reclamation.
//! * A create request the daemon completes *after* a reclaimer has already
//!   removed the scope — i.e. more than one TTL after the owner's last renewal —
//!   leaves a container with no record. The creator is dead or stalled past the
//!   TTL by then; there is no host-side primitive that fences a daemon request
//!   already in flight.
//! * A reclaimer that crashes between its claim and its re-read strands a live
//!   owner's (never checked-out) container until that claim's deadline passes;
//!   the next reclaimer then removes it. No Session is affected.
//! * A predecessor incarnation's records are indistinguishable from a live
//!   peer's until their deadline passes, so a restarted Worker reclaims them one
//!   TTL after the predecessor's last renewal, not immediately.

use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use awaken_time::{Expiry, UtcTime, WallClock};

use awaken_provisioning_contract as pc;
use serde::{Deserialize, Serialize};

use crate::{ContainerProvider, ContainerRuntime};

/// The scan that reads a warm lease record back. Named once so the claim that
/// minted the record and the reconciler that consumes it cannot disagree.
pub(crate) const WARM_LEASE_RECONCILER: &str = "reclaim_expired_capacity";

const LEASE_SCHEMA: u8 = 1;
const LEASE_SUFFIX: &str = "lease";
const MINT_SUFFIX: &str = "mint";
const CLAIM_PREFIX: &str = "claim.";

/// Validated lease timing. Validation is the shared
/// [`pc::LeaseTimingPolicy`]: renewal gets at least three opportunities per
/// TTL, and the recovery grace covers one TTL plus one reclamation interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WarmCapacityLeasePolicy {
    timing: pc::LeaseTimingPolicy,
}

impl WarmCapacityLeasePolicy {
    pub fn new(
        ttl: Duration,
        renew_interval: Duration,
        reclaim_interval: Duration,
    ) -> Result<Self, pc::SandboxError> {
        let ttl_ms = duration_ms(ttl);
        let reclaim_ms = duration_ms(reclaim_interval);
        pc::LeaseTimingPolicy::new(
            duration_ms(renew_interval),
            ttl_ms,
            reclaim_ms,
            ttl_ms.saturating_add(reclaim_ms),
        )
        .map(|timing| Self { timing })
        .map_err(|error| pc::SandboxError::invalid(format!("warm capacity lease timing: {error}")))
    }

    #[must_use]
    pub fn ttl(&self) -> Duration {
        Duration::from_millis(self.timing.lease_ttl_ms())
    }

    #[must_use]
    pub fn renew_interval(&self) -> Duration {
        Duration::from_millis(self.timing.renew_interval_ms())
    }

    #[must_use]
    pub fn reclaim_interval(&self) -> Duration {
        Duration::from_millis(self.timing.reconciliation_interval_ms())
    }
}

impl Default for WarmCapacityLeasePolicy {
    /// One-minute TTL, renewed every 15s, reclaimed every 30s.
    fn default() -> Self {
        Self::new(
            Duration::from_secs(60),
            Duration::from_secs(15),
            Duration::from_secs(30),
        )
        .expect("default warm capacity lease timing is valid")
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Where warm-capacity lease records live and how they are timed.
#[derive(Clone)]
pub struct WarmCapacityLeases {
    root: PathBuf,
    policy: WarmCapacityLeasePolicy,
    /// Mints and judges every lease deadline. Required: the Worker composition
    /// that owns the pool chooses it, so a test drives expiry with a manual
    /// clock instead of waiting and production never defaults silently.
    clock: Arc<dyn WallClock>,
    maintained: bool,
}

impl WarmCapacityLeases {
    /// Records under `root`, on `clock`, maintained by the pool.
    #[must_use]
    pub fn new(
        root: impl Into<PathBuf>,
        policy: WarmCapacityLeasePolicy,
        clock: Arc<dyn WallClock>,
    ) -> Self {
        Self {
            root: root.into(),
            policy,
            clock,
            maintained: true,
        }
    }

    /// The per-user host default: `<temp dir>/awaken-warm-capacity-leases-<uid>`,
    /// beside the retained host staging this crate already trusts across Worker
    /// replacement.
    pub fn host_default(clock: Arc<dyn WallClock>) -> Result<Self, pc::SandboxError> {
        let temp = std::env::temp_dir();
        let uid = effective_uid(&temp).map_err(|error| lease_error("host_default", error))?;
        Ok(Self::new(
            temp.join(format!("awaken-warm-capacity-leases-{uid}")),
            WarmCapacityLeasePolicy::default(),
            clock,
        ))
    }

    /// Do not spawn the renewal/reclamation loop. The caller then owns calling
    /// `renew_capacity_leases` within every renew interval and
    /// `reclaim_expired_capacity` — a pool left unrenewed loses its capacity to
    /// reclamation one TTL later, which is the safe direction.
    #[must_use]
    pub fn without_maintenance(mut self) -> Self {
        self.maintained = false;
        self
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn policy(&self) -> WarmCapacityLeasePolicy {
        self.policy
    }

    pub(crate) fn maintained(&self) -> bool {
        self.maintained
    }
}

/// What one reclamation pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WarmCapacityReclamation {
    /// Scopes whose expired record this pass claimed and whose container is
    /// now gone (removed, or already absent); their records are deleted.
    pub reclaimed: Vec<String>,
    /// Scopes this pass claimed but could not remove. The claim stays on disk
    /// with its own deadline, so a later pass retries.
    pub failed: Vec<String>,
    /// Scopes whose record looked expired but whose owner renewed before the
    /// claim: the record was put back untouched (L1).
    pub raced: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LeaseRecord {
    schema: u8,
    scope: String,
    holder: String,
    domain: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecordKind {
    Lease,
    Claim,
    Mint,
}

/// One record as a reclaimer saw it.
#[derive(Clone, Debug)]
pub(crate) struct Observed {
    path: PathBuf,
    pub(crate) scope: String,
    pub(crate) kind: RecordKind,
    pub(crate) deadline: Expiry,
}

/// The owner's view of its own record after a renew or release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Hold {
    /// The record is (was, for release) at its lease path and this operation
    /// applied to it.
    Held,
    /// The lease path no longer names this record: a reclaimer took it.
    Lost,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ClaimOutcome {
    /// This reclaimer now exclusively holds the record at `path`.
    Claimed(PathBuf),
    /// Another party consumed the record first (claim or check-out).
    Lost,
    /// The owner renewed before the claim; the record was put back.
    Live,
}

fn lease_error(operation: &'static str, error: impl std::fmt::Display) -> pc::SandboxError {
    pc::SandboxError::backend(operation, format!("warm capacity lease: {error}"))
}

/// Scopes and incarnations become file names, so they are held to a charset
/// with no separator, no dot and no path component.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// The mtime that persists `deadline`.
fn to_system_time(deadline: Expiry) -> SystemTime {
    UNIX_EPOCH + deadline.deadline().duration_since_epoch()
}

/// The deadline a record's mtime persists. A pre-epoch mtime reads as the
/// epoch, so such a record is already expired.
fn deadline_of(metadata: &fs::Metadata) -> io::Result<Expiry> {
    Ok(Expiry::at(UtcTime::from_system_time(metadata.modified()?)))
}

fn same_inode(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

/// The effective uid, read from a file this process just created, so no
/// platform call is needed and the answer is exactly the owner the kernel
/// stamps on what this process writes.
fn effective_uid(dir: &Path) -> io::Result<u32> {
    static PROBE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let probe = dir.join(format!(
        ".awaken-uid-probe-{}-{}",
        std::process::id(),
        PROBE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&probe)?;
    let uid = file.metadata().map(|metadata| metadata.uid());
    drop(file);
    let _ = fs::remove_file(&probe);
    uid
}

/// Create `dir` (0700) if absent and refuse it unless it is a real directory
/// owned by `uid` with none of the `forbidden` permission bits.
fn trusted_dir(dir: &Path, uid: u32, forbidden: u32) -> io::Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let metadata = fs::symlink_metadata(dir)?;
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & forbidden != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} must be a directory owned by uid {uid} without mode bits {forbidden:o}",
                dir.display()
            ),
        ));
    }
    Ok(())
}

/// The lease records of one lease domain, seen by one holder incarnation.
/// Every method is a short synchronous filesystem transition, so a record is
/// never half-written across an await.
pub(crate) struct LeaseStore {
    dir: PathBuf,
    uid: u32,
    domain: String,
    holder: String,
    ttl: Duration,
    clock: Arc<dyn WallClock>,
}

impl LeaseStore {
    pub(crate) fn open(
        config: &WarmCapacityLeases,
        domain: &str,
        holder: &str,
    ) -> Result<Self, pc::SandboxError> {
        if !valid_name(holder) || domain.is_empty() {
            return Err(pc::SandboxError::invalid(
                "warm capacity lease holder or domain is not a valid record name",
            ));
        }
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"awaken-warm-capacity-lease-domain/v1\0");
        hasher.update(domain.as_bytes());
        let domain_dir = config.root.join(&hasher.finalize().to_hex().as_str()[..32]);
        let prepare = || -> io::Result<u32> {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&config.root)?;
            let uid = effective_uid(&config.root)?;
            // The operator-chosen root may be shared with the user's group,
            // never writable by others; the domain directory this crate
            // creates is private.
            trusted_dir(&config.root, uid, 0o002)?;
            trusted_dir(&domain_dir, uid, 0o077)?;
            Ok(uid)
        };
        let uid = prepare().map_err(|error| lease_error("open", error))?;
        Ok(Self {
            dir: domain_dir,
            uid,
            domain: domain.to_owned(),
            holder: holder.to_owned(),
            ttl: config.policy.ttl(),
            clock: config.clock.clone(),
        })
    }

    #[cfg(test)]
    pub(crate) fn directory(&self) -> &Path {
        &self.dir
    }

    /// This store's reading of the lease clock: every deadline it mints and
    /// every expiry it judges comes from the same handle.
    pub(crate) fn reading(&self) -> UtcTime {
        self.clock.now()
    }

    fn deadline(&self) -> Expiry {
        Expiry::after(self.reading(), self.ttl)
    }

    fn lease_path(&self, scope: &str) -> PathBuf {
        self.dir.join(format!("{scope}.{LEASE_SUFFIX}"))
    }

    /// L4: publish the record for `scope` before anything can create it. The
    /// deadline is set on the unpublished inode, so no reader ever sees a
    /// lease file without its deadline; `link` refuses an existing name.
    pub(crate) fn mint(&self, scope: &str) -> io::Result<()> {
        if !valid_name(scope) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("warm scope {scope:?} is not a valid record name"),
            ));
        }
        let staged = self.dir.join(format!("{scope}.{MINT_SUFFIX}"));
        let record = serde_json::to_vec(&LeaseRecord {
            schema: LEASE_SCHEMA,
            scope: scope.to_owned(),
            holder: self.holder.clone(),
            domain: self.domain.clone(),
        })
        .map_err(io::Error::other)?;
        let publish = || -> io::Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&staged)?;
            file.write_all(&record)?;
            file.set_modified(to_system_time(self.deadline()))?;
            file.sync_all()?;
            fs::hard_link(&staged, self.lease_path(scope))
        };
        let mut published = publish();
        if matches!(&published, Err(error) if error.kind() == io::ErrorKind::NotFound) {
            // A peer's shutdown removed the (then empty) domain directory.
            // Re-establish it under the same trust check and publish once more.
            trusted_dir(&self.dir, self.uid, 0o077)?;
            published = publish();
        }
        let _ = fs::remove_file(&staged);
        published?;
        fs::File::open(&self.dir)?.sync_all()
    }

    /// Remove the domain directory if — and only if — it is empty, so a
    /// namespace that is never used again leaves no litter. `rmdir` refuses a
    /// directory holding any record, so this can never discard one.
    pub(crate) fn retire_directory_if_empty(&self) {
        let _ = fs::remove_dir(&self.dir);
    }

    /// L5: move the deadline of an existing record. Opening never creates, and
    /// the path is re-read afterwards, so a record a reclaimer renamed away is
    /// reported lost rather than silently renewed.
    pub(crate) fn renew(&self, scope: &str) -> io::Result<Hold> {
        let path = self.lease_path(scope);
        let file = match OpenOptions::new().write(true).open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Hold::Lost),
            Err(error) => return Err(error),
        };
        file.set_modified(to_system_time(self.deadline()))?;
        let renewed = file.metadata()?;
        match fs::symlink_metadata(&path) {
            Ok(current) if same_inode(&current, &renewed) => Ok(Hold::Held),
            Ok(_) => Ok(Hold::Lost),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Hold::Lost),
            Err(error) => Err(error),
        }
    }

    /// L3 (check-out) and L5 (release after disposal): consume the lease path.
    /// `Lost` means a reclaimer consumed it first.
    pub(crate) fn release(&self, scope: &str) -> io::Result<Hold> {
        match fs::remove_file(self.lease_path(scope)) {
            Ok(()) => Ok(Hold::Held),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Hold::Lost),
            Err(error) => Err(error),
        }
    }

    /// Every record in the domain with its deadline. Names that are not ours
    /// are ignored; a record that vanishes between listing and stat is skipped.
    pub(crate) fn observe(&self) -> io::Result<Vec<Observed>> {
        let mut observed = Vec::new();
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(observed),
            Err(error) => return Err(error),
        };
        for entry in entries {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some((scope, suffix)) = name.split_once('.') else {
                continue;
            };
            let kind = match suffix {
                LEASE_SUFFIX => RecordKind::Lease,
                MINT_SUFFIX => RecordKind::Mint,
                claim if claim.strip_prefix(CLAIM_PREFIX).is_some_and(valid_name) => {
                    RecordKind::Claim
                }
                _ => continue,
            };
            if !valid_name(scope) {
                continue;
            }
            let metadata = match fs::symlink_metadata(entry.path()) {
                Ok(metadata) if metadata.is_file() => metadata,
                Ok(_) => continue,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            observed.push(Observed {
                path: entry.path(),
                scope: scope.to_owned(),
                kind,
                deadline: deadline_of(&metadata)?,
            });
        }
        observed.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(observed)
    }

    /// L1/L2: take `observed` exclusively. The rename is the linearization
    /// point; the deadline re-read after it is what catches a renewal that
    /// landed between observation and rename, and such a record is put back
    /// unchanged (L1). The claim then carries this reclaimer's own deadline and
    /// is re-confirmed like a renewal, so a second reclaimer that raced for the
    /// same stale claim backs off instead of removing twice (L2c).
    pub(crate) fn claim(&self, observed: &Observed) -> io::Result<ClaimOutcome> {
        let claim = self
            .dir
            .join(format!("{}.{CLAIM_PREFIX}{}", observed.scope, self.holder));
        match fs::rename(&observed.path, &claim) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(ClaimOutcome::Lost);
            }
            Err(error) => return Err(error),
        }
        let taken = fs::symlink_metadata(&claim)?;
        if deadline_of(&taken)?.is_live_at(self.reading()) {
            fs::rename(&claim, &observed.path)?;
            return Ok(ClaimOutcome::Live);
        }
        let file = match OpenOptions::new().write(true).open(&claim) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(ClaimOutcome::Lost);
            }
            Err(error) => return Err(error),
        };
        file.set_modified(to_system_time(self.deadline()))?;
        let held = file.metadata()?;
        match fs::symlink_metadata(&claim) {
            Ok(current) if same_inode(&current, &held) => Ok(ClaimOutcome::Claimed(claim)),
            Ok(_) => Ok(ClaimOutcome::Lost),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(ClaimOutcome::Lost),
            Err(error) => Err(error),
        }
    }

    /// L5: the owner takes its *own* record exclusively before disposing what
    /// it names, through the same rename a reclaimer uses, so an owner and a
    /// reclaimer can never both remove one container. No liveness re-read: the
    /// owner is entitled to its record whatever its deadline. A claim this
    /// holder already left behind (an earlier disposal that failed) is reused.
    pub(crate) fn claim_own(&self, scope: &str) -> io::Result<ClaimOutcome> {
        let claim = self
            .dir
            .join(format!("{scope}.{CLAIM_PREFIX}{}", self.holder));
        match fs::rename(self.lease_path(scope), &claim) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if !claim.is_file() {
                    return Ok(ClaimOutcome::Lost);
                }
            }
            Err(error) => return Err(error),
        }
        let file = match OpenOptions::new().write(true).open(&claim) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(ClaimOutcome::Lost);
            }
            Err(error) => return Err(error),
        };
        file.set_modified(to_system_time(self.deadline()))?;
        let held = file.metadata()?;
        match fs::symlink_metadata(&claim) {
            Ok(current) if same_inode(&current, &held) => Ok(ClaimOutcome::Claimed(claim)),
            Ok(_) => Ok(ClaimOutcome::Lost),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(ClaimOutcome::Lost),
            Err(error) => Err(error),
        }
    }

    /// Delete a finished claim. Absence is success: nothing else can be
    /// waiting on a claim whose container is already gone.
    pub(crate) fn finish(&self, claim: &Path) -> io::Result<()> {
        match fs::remove_file(claim) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// A staged mint left by a creator that died before publishing it. No
    /// container can exist for it (the create starts only after the link), so
    /// it is removed once one TTL past its own deadline.
    fn stale_mint(&self, observed: &Observed, now: UtcTime) -> bool {
        observed.kind == RecordKind::Mint
            && Expiry::after(observed.deadline.deadline(), self.ttl).is_expired_at(now)
    }
}

/// The lease records one pool vouches for, over its [`LeaseStore`].
pub(crate) struct WarmLeaseLedger {
    store: LeaseStore,
    /// Scopes whose record this pool renews: minted and neither checked out,
    /// released after disposal, nor abandoned to expiry.
    held: Mutex<BTreeSet<String>>,
    /// One reclamation pass at a time per pool; passes of different pools are
    /// serialized by the claim rename instead.
    reclaiming: tokio::sync::Mutex<()>,
}

impl WarmLeaseLedger {
    pub(crate) fn new(store: LeaseStore) -> Self {
        Self {
            store,
            held: Mutex::new(BTreeSet::new()),
            reclaiming: tokio::sync::Mutex::new(()),
        }
    }

    #[cfg(test)]
    pub(crate) fn store(&self) -> &LeaseStore {
        &self.store
    }

    fn held(&self) -> std::sync::MutexGuard<'_, BTreeSet<String>> {
        self.held.lock().expect("warm lease ledger mutex")
    }

    /// L4: the record exists before this returns `Ok`.
    pub(crate) fn mint(&self, scope: &str) -> Result<(), pc::SandboxError> {
        self.store
            .mint(scope)
            .map_err(|error| lease_error("mint", error))?;
        self.held().insert(scope.to_owned());
        Ok(())
    }

    /// Stop renewing `scope`. Its record expires and reclamation removes
    /// whatever the scope names, idempotently.
    pub(crate) fn abandon(&self, scope: &str) {
        self.held().remove(scope);
    }

    /// Stop renewing everything. Used at the end of shutdown for what could
    /// not be disposed; a domain left with no record is removed.
    pub(crate) fn abandon_all(&self) {
        self.held().clear();
        self.store.retire_directory_if_empty();
    }

    /// L3: `true` only if this pool consumed the lease path, which is what
    /// makes the container unreachable to every reclaimer.
    pub(crate) fn check_out(&self, scope: &str) -> bool {
        self.abandon(scope);
        matches!(self.store.release(scope), Ok(Hold::Held))
    }

    /// L5: take this pool's own record before disposing its container.
    /// `None` means the record is gone to a reclaimer (who alone removes the
    /// container) or could not be taken; either way this pool must not remove.
    pub(crate) fn begin_disposal(&self, scope: &str) -> Option<PathBuf> {
        self.abandon(scope);
        match self.store.claim_own(scope) {
            Ok(ClaimOutcome::Claimed(claim)) => Some(claim),
            Ok(ClaimOutcome::Lost | ClaimOutcome::Live) | Err(_) => None,
        }
    }

    /// L5: a confirmed removal deletes the claim; a failed one leaves it to
    /// expire, after which any reclaimer (this pool included) finishes it.
    pub(crate) fn finish_disposal(&self, claim: &Path, removed: bool) {
        if removed {
            let _ = self.store.finish(claim);
        }
    }

    /// L5: renew every held record; returns the scopes found lost.
    pub(crate) fn renew_held(&self) -> Vec<String> {
        let scopes = self.held().iter().cloned().collect::<Vec<_>>();
        let mut lost = Vec::new();
        for scope in scopes {
            if let Ok(Hold::Lost) = self.store.renew(&scope) {
                self.abandon(&scope);
                lost.push(scope);
            }
        }
        lost
    }

    #[cfg(test)]
    pub(crate) fn held_scopes(&self) -> Vec<String> {
        self.held().iter().cloned().collect()
    }

    /// L1/L2/L2c/L6: one reclamation pass over the whole lease domain.
    pub(crate) async fn reclaim<R: ContainerRuntime + 'static>(
        &self,
        inner: &ContainerProvider<R>,
    ) -> Result<WarmCapacityReclamation, pc::SandboxError> {
        let _pass = self.reclaiming.lock().await;
        let now = self.store.reading();
        let mut report = WarmCapacityReclamation::default();
        let observed = self
            .store
            .observe()
            .map_err(|error| lease_error("reclaim", error))?;
        for record in observed {
            if self.store.stale_mint(&record, now) {
                let _ = fs::remove_file(&record.path);
                continue;
            }
            if record.kind == RecordKind::Mint || record.deadline.is_live_at(now) {
                continue;
            }
            let claim = match self.store.claim(&record) {
                Ok(ClaimOutcome::Claimed(claim)) => claim,
                Ok(ClaimOutcome::Lost) => continue,
                Ok(ClaimOutcome::Live) => {
                    report.raced.push(record.scope);
                    continue;
                }
                Err(_) => {
                    report.failed.push(record.scope);
                    continue;
                }
            };
            match inner.dispose_unreceipted_capacity(&record.scope).await {
                Ok(()) if self.store.finish(&claim).is_ok() => report.reclaimed.push(record.scope),
                Ok(()) | Err(_) => report.failed.push(record.scope),
            }
        }
        Ok(report)
    }
}
