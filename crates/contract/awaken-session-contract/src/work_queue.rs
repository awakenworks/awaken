//! The environment work-queue port + its neutral vocabulary (ADR self-hosted work).
//!
//! The port the environments work-queue routes drive, plus the neutral domain shapes
//! in its signatures. The in-memory reference backend and the shared lease
//! bookkeeping live outward in `awaken-work-store`, beside the sqlite/postgres
//! siblings. The Managed wire adapter owns the neutral→wire projection
//! (`WorkItem` → `BetaSelfHostedWork`); this crate names no wire type.

use std::collections::BTreeMap;

use async_trait::async_trait;
use awaken_agent_contract::RedactedString;

/// The frozen object timestamp the managed wire uses (single-machine builds have
/// no real clock in the *projection*; wire timestamps carry presence, not wall
/// time). Real wall time enters only as the `now_ms` argument the routes pass to
/// the lease/poll bookkeeping — never onto the wire — so the wire shape is
/// unchanged while leases can expire and pollers can be counted.
pub const OBJECT_AT: &str = "2026-01-01T00:00:00Z";

// The existing UTC wire vocabulary has four-digit years. Raw durable clocks
// retain their wider SQL domain; only calendar projection saturates here.
pub(crate) const MAX_RFC3339_NANOS: i128 = 253_402_300_799_999_999_999;

pub(crate) fn bounded_rfc3339_nanos(value: i128) -> i128 {
    value.clamp(0, MAX_RFC3339_NANOS)
}

/// Produce the opaque RFC-3339 receipt returned by a successful heartbeat.
///
/// Repeated writes in the same millisecond still receive distinct tokens, and a
/// wall clock that moves backwards cannot repeat an earlier receipt. Keeping this
/// pure conversion in the neutral queue contract lets every backend share the
/// exact compare-and-set vocabulary without importing a clock/date crate.
#[must_use]
pub fn next_heartbeat_receipt(now_ms: u64, previous: Option<&str>) -> String {
    const NANOS_PER_MILLI: i128 = 1_000_000;

    let requested = i128::from(now_ms)
        .saturating_mul(NANOS_PER_MILLI)
        .min(MAX_RFC3339_NANOS);
    let previous = previous.and_then(parse_rfc3339_utc_nanos);
    let next = match previous {
        Some(previous) if requested <= previous => previous.saturating_add(1),
        _ => requested,
    }
    .min(MAX_RFC3339_NANOS);
    format_rfc3339_utc_nanos(next)
}

pub(crate) fn parse_rfc3339_utc_nanos(value: &str) -> Option<i128> {
    let body = value.strip_suffix('Z')?;
    let (whole, fraction) = match body.split_once('.') {
        Some((_, "")) => return None,
        Some(parts) => parts,
        None => (body, ""),
    };
    let bytes = whole.as_bytes();
    if bytes.len() != 19
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let year = decimal(&bytes[0..4])? as i64;
    let month = decimal(&bytes[5..7])?;
    let day = decimal(&bytes[8..10])?;
    let hour = decimal(&bytes[11..13])?;
    let minute = decimal(&bytes[14..16])?;
    let second = decimal(&bytes[17..19])?;
    if year < 1970
        || !(1..=12).contains(&month)
        || !(1..=days_in_month(year, month)).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
        || fraction.len() > 9
    {
        return None;
    }
    let mut subsecond = 0_i128;
    for byte in fraction.bytes() {
        if !byte.is_ascii_digit() {
            return None;
        }
        subsecond = subsecond * 10 + i128::from(byte - b'0');
    }
    for _ in fraction.len()..9 {
        subsecond *= 10;
    }
    let days = days_from_civil(year, month, day);
    let seconds = i128::from(days) * 86_400
        + i128::from(hour) * 3_600
        + i128::from(minute) * 60
        + i128::from(second);
    Some(seconds * 1_000_000_000 + subsecond)
}

fn decimal(bytes: &[u8]) -> Option<u32> {
    bytes.iter().try_fold(0_u32, |value, byte| {
        byte.is_ascii_digit()
            .then(|| value * 10 + u32::from(*byte - b'0'))
    })
}

const fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let shifted_month = i64::from(month) + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

pub(crate) fn format_rfc3339_utc_nanos(total_nanos: i128) -> String {
    let total_nanos = bounded_rfc3339_nanos(total_nanos);
    let seconds = total_nanos / 1_000_000_000;
    let subsecond = total_nanos % 1_000_000_000;
    let days = i64::try_from(seconds / 86_400).unwrap_or(i64::MAX);
    let seconds_of_day = i64::try_from(seconds % 86_400).unwrap_or_default();
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day / 60) % 60;
    let second = seconds_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{subsecond:09}Z")
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = (month_prime + if month_prime < 10 { 3 } else { -9 }) as u32;
    year += i64::from(month <= 2);
    (year, month, day)
}

/// A work item's payload (the domain shape). The Managed wire adapter maps this onto
/// the tagged `BetaSelfHostedWork.data`; here it names no wire vocabulary. An
/// environment is seeded with a `HealthCheck`; a session assigned to a self-hosted
/// environment is enqueued as `Session` work (its inner `id` is the session id).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkPayload {
    HealthCheck { id: String },
    Session { id: String },
}

/// A work item's lifecycle state. `as_str` is the Anthropic wire vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkState {
    Queued,
    Starting,
    Active,
    /// No transition currently produces `Stopping` (`stop` goes straight to
    /// `Stopped`); it is kept because it is Anthropic wire vocabulary a future
    /// writer could emit, and `state_from_wire` must round-trip a `'stopping'` row.
    Stopping,
    Stopped,
}

impl WorkState {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Starting => "starting",
            Self::Active => "active",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
        }
    }

    /// Parse the wire/persisted string back to a state — the exact inverse of
    /// [`as_str`](Self::as_str), co-located here so the two CANNOT drift (adding a variant
    /// forces `as_str` to grow an arm, and this round-trips it). Returns `None` for an
    /// UNRECOGNIZED string rather than silently defaulting: a durable backend reading a
    /// corrupt or newer-schema state must fail CLOSED (treat it as terminal / not
    /// re-dispatchable), because silently mapping an unknown state to `Queued` would invite
    /// a re-claim and double execution — the exactly-once violation this inverse exists to
    /// prevent. See the round-trip property test in this crate.
    #[must_use]
    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "queued" => Some(Self::Queued),
            "starting" => Some(Self::Starting),
            "active" => Some(Self::Active),
            "stopping" => Some(Self::Stopping),
            "stopped" => Some(Self::Stopped),
            _ => None,
        }
    }

    /// Whether a poll may acquire this item.
    #[must_use]
    pub const fn is_claimable(self) -> bool {
        matches!(self, Self::Queued)
    }

    /// State after acknowledging receipt. An acknowledgement before a poll is a
    /// protocol-compatible `Queued -> Starting` transition; normal workers poll
    /// first, so acknowledging `Active` is an idempotent state no-op.
    #[must_use]
    pub const fn after_ack(self) -> Self {
        if matches!(self, Self::Queued) {
            Self::Starting
        } else {
            self
        }
    }

    /// Only the currently leased state can extend lease authority. A heartbeat
    /// may still return the current queued/starting/stopped state, but it must not
    /// manufacture a lease for an item that was never claimed.
    #[must_use]
    pub const fn can_extend_lease(self) -> bool {
        matches!(self, Self::Active)
    }

    /// Stop is absorbing at the work-item boundary.
    #[must_use]
    pub const fn after_stop(self) -> Self {
        Self::Stopped
    }
}

/// Storage-neutral result of admitting an owner/epoch guarded Work mutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkLeaseMutationAdmission {
    Applied,
    NotFound,
    PreconditionFailed,
}

/// One closed authorization table for Work acknowledgement, heartbeat, and
/// stop adapters. A backend obtains all facts from its existing atomic lock or
/// transaction, then this kernel decides whether any write may occur.
#[must_use]
pub const fn work_lease_mutation_admission(
    record_present: bool,
    owner_and_epoch_match: bool,
    command_condition_matches: bool,
) -> WorkLeaseMutationAdmission {
    if !record_present {
        WorkLeaseMutationAdmission::NotFound
    } else if !owner_and_epoch_match || !command_condition_matches {
        WorkLeaseMutationAdmission::PreconditionFailed
    } else {
        WorkLeaseMutationAdmission::Applied
    }
}

/// Storage-neutral plan for settling one exact Session Work lease. Durable
/// adapters obtain these facts from one locked row; the reference adapter
/// obtains owner, epoch, and lease presence from one LeaseBook snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionWorkReleaseAdmission {
    Apply,
    AlreadyReleased,
    Reject,
}

#[must_use]
pub const fn session_work_release_admission(
    identity_matches: bool,
    state_is_active: bool,
    state_is_stopped: bool,
    owner_matches: bool,
    epoch_matches: bool,
    authority_is_cleared: bool,
) -> SessionWorkReleaseAdmission {
    if identity_matches && state_is_active && owner_matches && epoch_matches {
        SessionWorkReleaseAdmission::Apply
    } else if identity_matches && state_is_stopped && epoch_matches && authority_is_cleared {
        SessionWorkReleaseAdmission::AlreadyReleased
    } else {
        SessionWorkReleaseAdmission::Reject
    }
}

/// One queued/leased unit of work in an environment's queue (the domain shape; the
/// Managed adapter renders the `BetaSelfHostedWork` wire object from it).
#[derive(Clone, Debug)]
pub struct WorkItem {
    pub id: String,
    pub environment_id: String,
    pub data: WorkPayload,
    pub metadata: BTreeMap<String, String>,
    pub state: WorkState,
    pub acknowledged_at: Option<String>,
    pub latest_heartbeat_at: Option<String>,
    pub started_at: Option<String>,
    pub stop_requested_at: Option<String>,
    pub stopped_at: Option<String>,
}

/// A freshly claimed Work item plus the one-time per-Session bearer minted by
/// the authoritative WorkQueue transaction. The cleartext is returned only by
/// `poll`; list/retrieve projections continue to expose `secret: null`.
#[derive(Clone, Debug)]
pub struct ClaimedWork {
    pub item: WorkItem,
    pub sessions_token: Option<RedactedString>,
}

/// Current Work lease recovered from a presented per-Session bearer.
///
/// The token is not a Worker identity or a second lease. This projection
/// carries the exact owner/epoch already persisted by the WorkQueue so protocol
/// adapters can authorize downstream Session calls and map heartbeat/stop back
/// onto the same lease owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkSessionAccess {
    pub work_id: String,
    pub environment_id: String,
    pub session_id: crate::SessionId,
    pub lease_owner: String,
    pub lease_epoch: u64,
    pub expires_at_unix_ms: u64,
}

/// Durable authority currently binding one Session work item to one Worker.
/// This value is intentionally not part of the Managed wire object: public
/// workers use the standard header/heartbeat protocol, while Awaken's private
/// Run ingress consumes it only as a subordinate ownership fence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionWorkLease {
    pub work_id: String,
    pub environment_id: String,
    pub session_id: crate::SessionId,
    /// Exact authenticated execution owner. For a registered Worker this is
    /// its incarnation-qualified lease owner, not the reusable display id.
    pub owner: String,
    pub epoch: u64,
    pub expires_at_unix_ms: u64,
}

/// Exact, secret-free proof of the Work authority observed when a registered
/// Worker entered one Session attempt.
///
/// The fields are private so production callers cannot assemble authority from
/// unrelated strings or silently replace a stale epoch with a later one. Wire
/// input is still untrusted: the Coordinator always compares this receipt with
/// a fresh observation from the canonical WorkQueue before exposing committed
/// Thread truth or admitting a Session effect.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionWorkAuthorityReceipt {
    session_id: crate::SessionId,
    worker_owner: String,
    authority: SessionWorkAuthorityCoordinate,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SessionWorkAuthorityCoordinate {
    NotRequired,
    Leased {
        work_id: String,
        environment_id: String,
        epoch: u64,
    },
}

const fn exact_session_work_coordinate(
    session_matches: bool,
    owner_matches: bool,
    scope_matches: bool,
    work_matches: bool,
    environment_matches: bool,
    epoch_matches: bool,
) -> bool {
    session_matches
        && owner_matches
        && scope_matches
        && work_matches
        && environment_matches
        && epoch_matches
}

impl SessionWorkAuthorityReceipt {
    /// Issue a receipt only from the complete result returned by the canonical
    /// acquisition port. `Unowned` and a foreign owner cannot mint authority.
    #[must_use]
    fn from_acquired(
        session_id: &str,
        worker_owner: &str,
        ownership: &SessionWorkOwnership,
    ) -> Option<Self> {
        let authority = match ownership {
            SessionWorkOwnership::NotRequired => SessionWorkAuthorityCoordinate::NotRequired,
            SessionWorkOwnership::Unowned => return None,
            SessionWorkOwnership::Leased(lease)
                if lease.session_id == session_id && lease.owner == worker_owner =>
            {
                SessionWorkAuthorityCoordinate::Leased {
                    work_id: lease.work_id.clone(),
                    environment_id: lease.environment_id.clone(),
                    epoch: lease.epoch,
                }
            }
            SessionWorkOwnership::Leased(_) => return None,
        };
        Some(Self {
            session_id: crate::SessionId::from(session_id),
            worker_owner: worker_owner.to_string(),
            authority,
        })
    }

    #[must_use]
    pub fn session_id(&self) -> &str {
        self.session_id.as_str()
    }

    #[must_use]
    pub fn worker_owner(&self) -> &str {
        &self.worker_owner
    }

    /// Compare every stable coordinate with a new canonical observation. Lease
    /// expiry is deliberately omitted: renewal may extend time, while Work id,
    /// Environment, Session, owner, and epoch must remain identical.
    #[must_use]
    pub fn matches_observation(&self, ownership: &SessionWorkOwnership) -> bool {
        match (&self.authority, ownership) {
            (SessionWorkAuthorityCoordinate::NotRequired, SessionWorkOwnership::NotRequired) => {
                exact_session_work_coordinate(true, true, true, true, true, true)
            }
            (
                SessionWorkAuthorityCoordinate::Leased {
                    work_id,
                    environment_id,
                    epoch,
                },
                SessionWorkOwnership::Leased(lease),
            ) => exact_session_work_coordinate(
                self.session_id == lease.session_id,
                self.worker_owner == lease.owner,
                true,
                work_id == &lease.work_id,
                environment_id == &lease.environment_id,
                *epoch == lease.epoch,
            ),
            _ => false,
        }
    }

    /// Bind this Work observation to the realized Session directive without
    /// copying either authority. A leased receipt must name the directive's
    /// self-hosted Environment and exact runtime incarnation; a NotRequired
    /// receipt is valid only for a non-self-hosted projection.
    #[must_use]
    pub fn binds_realization(
        &self,
        session_id: &str,
        directive: &crate::SessionRealizationDirective,
    ) -> bool {
        let environment = &directive.projection.baseline.environment;
        let (scope_matches, environment_matches) = match &self.authority {
            SessionWorkAuthorityCoordinate::NotRequired => (!environment.self_hosted, true),
            SessionWorkAuthorityCoordinate::Leased { environment_id, .. } => (
                environment.self_hosted,
                environment_id == &environment.environment_id,
            ),
        };
        exact_session_work_coordinate(
            self.session_id == session_id,
            self.worker_owner == directive.authority.lease.runtime_incarnation,
            scope_matches,
            true,
            environment_matches,
            true,
        )
    }
}

/// Acquire through the one Work authority and seal its complete stable
/// coordinate into a receipt. Callers cannot construct or alter the typed
/// receipt through Rust fields; a decoded wire value remains only an assertion
/// until [`SessionWorkAuthorityReceipt::matches_observation`] succeeds.
pub async fn acquire_session_work_receipt(
    authority: &dyn SessionWorkLeaseAuthority,
    session_id: &crate::SessionId,
    worker_owner: &str,
    now_ms: u64,
    acquisition: SessionWorkAcquisition,
) -> Result<Option<SessionWorkAuthorityReceipt>, WorkQueueError> {
    let ownership = authority
        .acquire_session_work(session_id, worker_owner, now_ms, acquisition)
        .await?;
    Ok(SessionWorkAuthorityReceipt::from_acquired(
        session_id.as_str(),
        worker_owner,
        &ownership,
    ))
}

#[cfg(kani)]
#[kani::proof]
fn exact_session_work_receipt_rejects_every_changed_coordinate() {
    let session_matches: bool = kani::any();
    let owner_matches: bool = kani::any();
    let scope_matches: bool = kani::any();
    let work_matches: bool = kani::any();
    let environment_matches: bool = kani::any();
    let epoch_matches: bool = kani::any();
    assert_eq!(
        exact_session_work_coordinate(
            session_matches,
            owner_matches,
            scope_matches,
            work_matches,
            environment_matches,
            epoch_matches,
        ),
        session_matches
            && owner_matches
            && scope_matches
            && work_matches
            && environment_matches
            && epoch_matches
    );
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionWorkOwnership {
    NotRequired,
    Unowned,
    Leased(SessionWorkLease),
}

#[derive(Clone, Copy)]
enum SessionWorkEffectAuthority {
    NotRequired,
    Unowned,
    Leased,
}

const fn session_work_effect_policy(
    authority: SessionWorkEffectAuthority,
    owner_matches: bool,
) -> bool {
    match authority {
        SessionWorkEffectAuthority::NotRequired => true,
        SessionWorkEffectAuthority::Unowned => false,
        SessionWorkEffectAuthority::Leased => owner_matches,
    }
}

impl SessionWorkOwnership {
    /// Whether this observed Work authority permits the authenticated Worker
    /// to enter or renew Session realization effects.
    #[must_use]
    pub fn permits_effect_for(&self, worker_owner: &str) -> bool {
        let (authority, owner_matches) = match self {
            Self::NotRequired => (SessionWorkEffectAuthority::NotRequired, false),
            Self::Unowned => (SessionWorkEffectAuthority::Unowned, false),
            Self::Leased(lease) => (
                SessionWorkEffectAuthority::Leased,
                lease.owner == worker_owner,
            ),
        };
        session_work_effect_policy(authority, owner_matches)
    }
}

/// Cause authorizing a private registered Worker to acquire Session Work.
/// A live claimed Run may repair a stopped Work projection after an admission /
/// predecessor-retirement race. Realization renewal carries no driving event
/// and may only renew or acquire already-dispatched Work; it cannot resurrect a
/// stopped Session after its Run has settled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionWorkAcquisition {
    ClaimedRun,
    RealizationRenewal,
}

/// Whether an absent Session Work lease may be repaired through the canonical
/// wake path. A live claimed Run is the only durable intent that can authorize
/// revival; renewal and terminal Session observations must remain inert.
#[must_use]
pub const fn session_work_revival_is_authorized(
    acquisition: SessionWorkAcquisition,
    session_is_terminal: bool,
    lease_was_acquired: bool,
) -> bool {
    !lease_was_acquired
        && !session_is_terminal
        && matches!(acquisition, SessionWorkAcquisition::ClaimedRun)
}

#[cfg(kani)]
#[kani::proof]
fn stopped_session_work_is_revived_only_by_a_claimed_nonterminal_run() {
    let session_is_terminal: bool = kani::any();
    let lease_was_acquired: bool = kani::any();
    let claimed_run: bool = kani::any();
    let acquisition = if claimed_run {
        SessionWorkAcquisition::ClaimedRun
    } else {
        SessionWorkAcquisition::RealizationRenewal
    };

    assert_eq!(
        session_work_revival_is_authorized(acquisition, session_is_terminal, lease_was_acquired,),
        claimed_run && !session_is_terminal && !lease_was_acquired
    );
}

/// Coordinator-side view that subordinates private Run execution to the one
/// public Environment Work ownership decision without exposing the Work store.
#[async_trait]
pub trait SessionWorkLeaseAuthority: Send + Sync {
    /// Notify the existing Session lifecycle supervisor that Worker placement
    /// evidence changed. This is an edge-triggered hint only: the supervisor
    /// re-reads the Session, WorkQueue, and Worker registry authorities before
    /// performing any mutation or effect.
    fn request_lifecycle_reconciliation(&self) {}

    async fn acquire_session_work(
        &self,
        session_id: &crate::SessionId,
        worker_owner: &str,
        now_ms: u64,
        acquisition: SessionWorkAcquisition,
    ) -> Result<SessionWorkOwnership, WorkQueueError>;

    /// Release the exact Session Work lease after the registered Worker's Run
    /// has committed and is ready to settle. `false` means the caller no longer
    /// owns that Session Work and grants no further execution authority. A
    /// current Dispatch claim may still close Done when its exact Run has
    /// committed Ended truth; terminal cleanup deliberately retires Work before
    /// waiting for that closure. Other settlement requires the live Work fence.
    async fn release_session_work(&self, lease: &SessionWorkLease) -> Result<bool, WorkQueueError>;

    /// Return every non-terminal Session Work lease owned by one exact registered
    /// Worker incarnation to the queue during authority loss or deregistration.
    async fn release_worker_session_work(
        &self,
        worker_owner: &str,
    ) -> Result<usize, WorkQueueError>;
}

/// Result of a mutation that only the current work owner may perform.
#[derive(Clone, Debug)]
pub enum WorkMutationResult {
    Accepted(Box<WorkItem>),
    PreconditionFailed,
    NotFound,
}

impl WorkMutationResult {
    #[must_use]
    pub fn accepted(work: WorkItem) -> Self {
        Self::Accepted(Box::new(work))
    }

    #[must_use]
    pub fn into_item(self) -> Option<WorkItem> {
        match self {
            Self::Accepted(item) => Some(*item),
            Self::PreconditionFailed | Self::NotFound => None,
        }
    }

    #[must_use]
    pub const fn is_not_found(&self) -> bool {
        matches!(self, Self::NotFound)
    }

    #[must_use]
    pub const fn is_accepted(&self) -> bool {
        matches!(self, Self::Accepted(_))
    }
}

/// Optimistic concurrency condition supplied by a worker heartbeat.
///
/// The Managed wire uses `NO_HEARTBEAT` for the first heartbeat and then asks
/// the worker to echo the preceding receipt exactly. Keeping that vocabulary at
/// the adapter edge while representing its meaning here makes every store use
/// the same comparison rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeartbeatCondition {
    /// Backward-compatible/manual calls which did not request a conditional write.
    Unconditional,
    /// The item must not have recorded any heartbeat in its current lease.
    First,
    /// The latest heartbeat must equal this opaque receipt exactly.
    Matching(String),
}

impl HeartbeatCondition {
    /// Build the domain condition from the optional Managed query parameter.
    #[must_use]
    pub fn from_wire(expected: Option<&str>) -> Self {
        match expected {
            None => Self::Unconditional,
            Some("NO_HEARTBEAT") => Self::First,
            Some(value) => Self::Matching(value.to_string()),
        }
    }

    /// Whether `actual` authorizes this heartbeat write.
    #[must_use]
    pub fn permits(&self, actual: Option<&str>) -> bool {
        match self {
            Self::Unconditional => true,
            Self::First => actual.is_none(),
            Self::Matching(expected) => actual == Some(expected.as_str()),
        }
    }
}

/// One heartbeat mutation request. `desired_ttl_seconds` is optional on the
/// Managed wire; the store applies its documented default when absent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseHeartbeat {
    pub condition: HeartbeatCondition,
    pub desired_ttl_seconds: Option<u64>,
}

impl LeaseHeartbeat {
    #[must_use]
    pub const fn unconditional() -> Self {
        Self {
            condition: HeartbeatCondition::Unconditional,
            desired_ttl_seconds: None,
        }
    }
}

/// The neutral heartbeat receipt (the port's shape): the lease was extended and
/// its TTL. The route projects this to the wire `WorkHeartbeat` (adding the
/// `object_type` tag).
#[derive(Debug, Clone)]
pub struct LeaseReceipt {
    /// Opaque RFC-3339 compare token to echo on the next heartbeat.
    pub last_heartbeat: String,
    pub lease_extended: bool,
    pub state: WorkState,
    pub ttl_seconds: u64,
}

/// Result of an atomic heartbeat compare-and-extend operation.
#[derive(Debug, Clone)]
pub enum HeartbeatResult {
    Accepted(LeaseReceipt),
    PreconditionFailed,
    NotFound,
}

impl HeartbeatResult {
    #[must_use]
    pub fn into_receipt(self) -> Option<LeaseReceipt> {
        match self {
            Self::Accepted(receipt) => Some(receipt),
            Self::PreconditionFailed | Self::NotFound => None,
        }
    }

    #[must_use]
    pub const fn is_not_found(&self) -> bool {
        matches!(self, Self::NotFound)
    }
}

/// The neutral queue statistics (the port's shape): depth, in-flight count, the
/// oldest unfinished item's timestamp, and the live poller count. The route
/// projects this to the wire `WorkQueueStats`.
#[derive(Debug, Clone)]
pub struct QueueStats {
    pub depth: usize,
    pub pending: usize,
    pub oldest_queued_at: Option<String>,
    pub workers_polling: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkQueueError {
    #[error("work queue storage failure: {0}")]
    Storage(String),
}

/// The port the environments work-queue routes drive. In-memory by default; a
/// durable impl (sqlite / postgres) backs it at parity. Membership is enforced by
/// the port: an operation on a `wid` that does not belong to `env_id` returns
/// `None`, which the route maps to a `work not found` 404.
#[async_trait]
pub trait WorkQueue: Send + Sync {
    /// Ensure one `session` work item exists for `(env_id, session_id)` and
    /// return its canonical work id. Replays are idempotent so a durable Session
    /// reconciliation loop can safely close a commit-before-dispatch crash gap.
    async fn enqueue_session(
        &self,
        env_id: &str,
        session_id: &crate::SessionId,
    ) -> Result<String, WorkQueueError>;
    /// Explicitly wake an existing stopped Session work item, or create it when
    /// absent. Unlike reconciliation's [`Self::enqueue_session`], this command
    /// is driven only by a new admitted Session event, so a completed idle item
    /// cannot be continuously resurrected by the background supervisor.
    async fn wake_session(
        &self,
        env_id: &str,
        session_id: &crate::SessionId,
    ) -> Result<String, WorkQueueError>;
    /// Seed a `healthcheck` work item (its inner id is the work id); returns it.
    async fn enqueue_healthcheck(&self, env_id: &str) -> Result<String, WorkQueueError>;
    /// Return the one healthcheck for an Environment, creating it when absent.
    /// This convergence operation is safe to replay after an Environment create
    /// committed but the caller lost the response.
    async fn ensure_healthcheck(&self, env_id: &str) -> Result<String, WorkQueueError>;
    /// All work items in `env_id`, ascending by id (enqueue order).
    async fn list(&self, env_id: &str) -> Result<Vec<WorkItem>, WorkQueueError>;
    /// The work item under `wid` when it belongs to `env_id`.
    async fn get(&self, env_id: &str, wid: &str) -> Result<Option<WorkItem>, WorkQueueError>;
    /// Poll as `worker_id` at wall time `now_ms`: first reclaim any `active` item
    /// whose lease has lapsed (its worker went away), then lease the oldest queued
    /// item (queued→active) when none is actively leased. `None` when the queue is
    /// empty or one is still live-leased. The poll is recorded for `workers_polling`.
    async fn claim(
        &self,
        env_id: &str,
        worker_id: &str,
        now_ms: u64,
    ) -> Result<Option<WorkItem>, WorkQueueError>;
    /// Claim with an optional caller-requested reclaim age. Backends that cannot
    /// tune lease clocks may conservatively delegate to [`Self::claim`].
    async fn claim_with_reclaim(
        &self,
        env_id: &str,
        lease_owner: &str,
        poller_id: &str,
        now_ms: u64,
        reclaim_older_than_ms: Option<u64>,
    ) -> Result<Option<WorkItem>, WorkQueueError> {
        let _ = reclaim_older_than_ms;
        let _ = poller_id;
        self.claim(env_id, lease_owner, now_ms).await
    }
    /// Claim through the public self-hosted Worker protocol and atomically mint
    /// a per-Session bearer when the selected item is Session work. Durable
    /// implementations persist only its digest beside the lease and return the
    /// cleartext once in [`ClaimedWork`].
    async fn claim_with_session_access(
        &self,
        env_id: &str,
        lease_owner: &str,
        poller_id: &str,
        now_ms: u64,
        reclaim_older_than_ms: Option<u64>,
    ) -> Result<Option<ClaimedWork>, WorkQueueError> {
        Ok(self
            .claim_with_reclaim(
                env_id,
                lease_owner,
                poller_id,
                now_ms,
                reclaim_older_than_ms,
            )
            .await?
            .map(|item| ClaimedWork {
                item,
                sessions_token: None,
            }))
    }
    /// Authenticate one presented per-Session bearer against the current live
    /// Work lease. Expired, reclaimed, stopped, retired, or replaced leases
    /// return `None`; implementations never retain the cleartext.
    async fn authenticate_session_access(
        &self,
        _presented: &RedactedString,
        _now_ms: u64,
    ) -> Result<Option<WorkSessionAccess>, WorkQueueError> {
        Ok(None)
    }
    /// Apply an acknowledgement only while the exact lease epoch recovered
    /// from a sessions token is still current. Backends that do not issue
    /// sessions tokens fail closed unless they explicitly implement this seam.
    async fn ack_with_session_access(
        &self,
        _access: &WorkSessionAccess,
    ) -> Result<WorkMutationResult, WorkQueueError> {
        Ok(WorkMutationResult::PreconditionFailed)
    }
    /// Extend only the exact lease epoch recovered from a sessions token.
    async fn heartbeat_with_session_access(
        &self,
        _access: &WorkSessionAccess,
        _now_ms: u64,
        _heartbeat: LeaseHeartbeat,
    ) -> Result<HeartbeatResult, WorkQueueError> {
        Ok(HeartbeatResult::PreconditionFailed)
    }
    /// Stop only the exact lease epoch recovered from a sessions token.
    async fn stop_with_session_access(
        &self,
        _access: &WorkSessionAccess,
    ) -> Result<WorkMutationResult, WorkQueueError> {
        Ok(WorkMutationResult::PreconditionFailed)
    }
    /// Acknowledge receipt (queued→starting), stamping `acknowledged_at`.
    async fn ack(
        &self,
        env_id: &str,
        wid: &str,
        worker_id: &str,
    ) -> Result<WorkMutationResult, WorkQueueError>;
    /// Atomically compare the preceding heartbeat and, when it matches, record a
    /// new heartbeat at `now_ms` and extend the lease.
    async fn heartbeat(
        &self,
        env_id: &str,
        wid: &str,
        worker_id: &str,
        now_ms: u64,
        heartbeat: LeaseHeartbeat,
    ) -> Result<HeartbeatResult, WorkQueueError>;
    /// Request a stop (→stopped).
    async fn stop(
        &self,
        env_id: &str,
        wid: &str,
        worker_id: &str,
    ) -> Result<WorkMutationResult, WorkQueueError>;
    /// Registered-Worker authority teardown. Only active Session items with the
    /// exact incarnation-qualified owner are returned to the queue, so a live
    /// non-terminal Session can be adopted by a fresh incarnation.
    async fn release_owner(&self, worker_owner: &str) -> Result<usize, WorkQueueError>;
    /// Coordinator-owned terminal projection. This is not a Worker mutation:
    /// it retires the canonical item after the Session terminal fence and
    /// therefore intentionally does not require the former lease owner.
    async fn retire_session(
        &self,
        env_id: &str,
        session_id: &crate::SessionId,
    ) -> Result<Option<WorkItem>, WorkQueueError>;
    /// Registered-Worker settlement transition. Retire only the exact current
    /// Session Work owner and lease epoch in one store compare-and-set; a stale
    /// request must never retire a successor lease from the same incarnation.
    async fn release_session(&self, _lease: &SessionWorkLease) -> Result<bool, WorkQueueError> {
        Ok(false)
    }
    /// Private registered-Worker adapter over the same WorkQueue authority.
    /// It claims the exact queued Session or renews that exact current owner.
    /// When the exact Session is actively held by another owner, return that
    /// current lease so the caller can distinguish stale ownership from a
    /// retired or capacity-blocked item. The Environment single-active cap and
    /// non-claimable states otherwise fail closed with `None`.
    async fn acquire_session(
        &self,
        env_id: &str,
        session_id: &crate::SessionId,
        worker_owner: &str,
        now_ms: u64,
    ) -> Result<Option<SessionWorkLease>, WorkQueueError>;
    /// Merge a metadata patch (a string upserts and None deletes the key).
    async fn update_metadata(
        &self,
        env_id: &str,
        wid: &str,
        patch: BTreeMap<String, Option<String>>,
    ) -> Result<Option<WorkItem>, WorkQueueError>;
    /// Queue stats for `env_id` as of `now_ms` (for the `workers_polling` window).
    async fn stats(&self, env_id: &str, now_ms: u64) -> Result<QueueStats, WorkQueueError>;
    /// Drop all work for `env_id` (on environment delete).
    async fn remove_env(&self, env_id: &str) -> Result<(), WorkQueueError>;
}

#[cfg(test)]
mod tests {
    use super::{
        SessionWorkLease, SessionWorkOwnership, SessionWorkReleaseAdmission,
        WorkLeaseMutationAdmission, next_heartbeat_receipt, session_work_release_admission,
        work_lease_mutation_admission,
    };

    #[test]
    fn session_work_effect_admission_follows_the_complete_authority_table() {
        // Cause/effect graph: C1 the frozen Session requires no external Work;
        // C2 required Work is absent/retired or leased; C3 the observed lease
        // names the authenticated Worker. Effects: E1 permit C1 and exact
        // C2+C3; E2 deny absent Work and a foreign owner. The four rows cover
        // every semantic observation produced by SessionWorkLeaseAuthority;
        // lease identity/epoch validity remains inside that authoritative
        // queue read.
        //
        // | Rule | Work observation | Owner | Permitted |
        // |---|---|---|---|
        // | A1 | NotRequired | any | yes |
        // | A2 | Unowned | any | no |
        // | A3 | Leased | exact | yes |
        // | A4 | Leased | foreign | no |
        let lease = |owner: &str| SessionWorkLease {
            work_id: "work".into(),
            environment_id: "environment".into(),
            session_id: "session".into(),
            owner: owner.into(),
            epoch: 7,
            expires_at_unix_ms: 11,
        };
        for (rule, ownership, expected) in [
            ("A1", SessionWorkOwnership::NotRequired, true),
            ("A2", SessionWorkOwnership::Unowned, false),
            ("A3", SessionWorkOwnership::Leased(lease("worker")), true),
            ("A4", SessionWorkOwnership::Leased(lease("foreign")), false),
        ] {
            assert_eq!(ownership.permits_effect_for("worker"), expected, "{rule}");
        }
    }

    #[test]
    fn utc_calendar_codec_covers_every_representable_day() {
        // D6 exhaustive 1970..9999 day-domain coverage. Check both calendar
        // directions, real month lengths, and dense successive dates. Golden
        // Unix date anchors live in run_application::tests to avoid a solely
        // self-consistent round-trip oracle.
        for day in 0..=2_932_896 {
            let (year, month, date) = super::civil_from_days(day);
            assert!((1970..=9999).contains(&year));
            assert!((1..=12).contains(&month));
            assert!((1..=super::days_in_month(year, month)).contains(&date));
            assert_eq!(super::days_from_civil(year, month, date), day);
        }
        assert_eq!(super::civil_from_days(2_932_896), (9999, 12, 31));
    }

    #[test]
    fn heartbeat_receipts_are_rfc3339_and_advance_within_one_millisecond() {
        let first = next_heartbeat_receipt(0, None);
        let second = next_heartbeat_receipt(0, Some(&first));
        assert_eq!(first, "1970-01-01T00:00:00.000000000Z");
        assert_eq!(second, "1970-01-01T00:00:00.000000001Z");
    }

    #[test]
    fn heartbeat_receipts_do_not_go_backwards_with_the_wall_clock() {
        let previous = "2026-01-01T00:00:00.999999999Z";
        assert_eq!(
            next_heartbeat_receipt(0, Some(previous)),
            "2026-01-01T00:00:01.000000000Z"
        );
    }

    #[test]
    fn heartbeat_receipts_accept_the_old_seconds_only_shape() {
        assert_eq!(
            next_heartbeat_receipt(0, Some("2026-01-01T00:00:00Z")),
            "2026-01-01T00:00:00.000000001Z"
        );
    }

    #[test]
    fn lease_mutation_admission_follows_the_complete_decision_table() {
        // Causes: C1 the Work row exists; C2 exact owner+epoch matches; C3 the
        // command CAS condition matches. Effects: E1 only C1+C2+C3 mutates;
        // E2 absence is distinguishable; E3 every stale fact fails unchanged.
        // Decision rules are the complete 2^3 input table below.
        for present in [false, true] {
            for authority in [false, true] {
                for condition in [false, true] {
                    let actual = work_lease_mutation_admission(present, authority, condition);
                    let expected = if !present {
                        WorkLeaseMutationAdmission::NotFound
                    } else if authority && condition {
                        WorkLeaseMutationAdmission::Applied
                    } else {
                        WorkLeaseMutationAdmission::PreconditionFailed
                    };
                    assert_eq!(
                        actual, expected,
                        "C1={present} C2={authority} C3={condition}"
                    );
                }
            }
        }
    }

    #[test]
    fn session_work_release_follows_the_complete_authority_table() {
        // Causes: C1 exact Work/Environment/Session identity; C2 Active or
        // Stopped state; C3 exact owner; C4 exact epoch; C5 authority cleared.
        // Effects: E1 Apply only for exact active owner+epoch; E2 idempotent
        // success only for exact stopped epoch with no owner/lease; E3 every
        // other combination rejects unchanged. This exhausts all 2^6 rules.
        for identity in [false, true] {
            for active in [false, true] {
                for stopped in [false, true] {
                    for owner in [false, true] {
                        for epoch in [false, true] {
                            for cleared in [false, true] {
                                let actual = session_work_release_admission(
                                    identity, active, stopped, owner, epoch, cleared,
                                );
                                let expected = if identity && active && owner && epoch {
                                    SessionWorkReleaseAdmission::Apply
                                } else if identity && stopped && epoch && cleared {
                                    SessionWorkReleaseAdmission::AlreadyReleased
                                } else {
                                    SessionWorkReleaseAdmission::Reject
                                };
                                assert_eq!(actual, expected);
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    #[kani::proof]
    fn session_work_effect_requires_not_required_scope_or_exact_owner() {
        // Complete policy domain: every closed Work-authority class crossed
        // with both exact-owner facts. The production wrapper above performs
        // String equality once and feeds that fact to this sole policy owner;
        // native and real-router tests cross-check the wrapper with non-empty
        // exact and foreign owners. No assumption or stub removes a rule.
        let authority = match kani::any::<u8>() % 3 {
            0 => SessionWorkEffectAuthority::NotRequired,
            1 => SessionWorkEffectAuthority::Unowned,
            _ => SessionWorkEffectAuthority::Leased,
        };
        let owner_matches: bool = kani::any();
        let actual = session_work_effect_policy(authority, owner_matches);
        let expected = matches!(authority, SessionWorkEffectAuthority::NotRequired)
            || (matches!(authority, SessionWorkEffectAuthority::Leased) && owner_matches);

        assert_eq!(
            actual, expected,
            "effects require a Work-free scope or the exact current Work owner"
        );
    }

    fn symbolic_state(tag: u8) -> WorkState {
        match tag % 5 {
            0 => WorkState::Queued,
            1 => WorkState::Starting,
            2 => WorkState::Active,
            3 => WorkState::Stopping,
            _ => WorkState::Stopped,
        }
    }

    #[kani::proof]
    fn only_queued_work_is_claimable() {
        let state = symbolic_state(kani::any());
        assert_eq!(state.is_claimable(), state == WorkState::Queued);
    }

    #[kani::proof]
    fn only_active_work_accepts_lease_extension() {
        let state = symbolic_state(kani::any());
        assert_eq!(state.can_extend_lease(), state == WorkState::Active);
    }

    #[kani::proof]
    fn work_lease_mutation_requires_present_exact_authority_and_matching_condition() {
        let present = kani::any::<bool>();
        let authority = kani::any::<bool>();
        let condition = kani::any::<bool>();
        let decision = work_lease_mutation_admission(present, authority, condition);
        assert_eq!(
            decision == WorkLeaseMutationAdmission::Applied,
            present && authority && condition
        );
        assert_eq!(decision == WorkLeaseMutationAdmission::NotFound, !present);
    }

    #[kani::proof]
    fn session_work_release_requires_exact_owner_and_epoch() {
        // Same complete C1-C5/E1-E3 table as the native test. The asserted
        // formula is independent of the production decision implementation.
        let identity: bool = kani::any();
        let active: bool = kani::any();
        let stopped: bool = kani::any();
        let owner: bool = kani::any();
        let epoch: bool = kani::any();
        let cleared: bool = kani::any();
        kani::assume(!(active && stopped));
        let decision =
            session_work_release_admission(identity, active, stopped, owner, epoch, cleared);
        assert_eq!(
            decision == SessionWorkReleaseAdmission::Apply,
            identity && active && owner && epoch
        );
        assert_eq!(
            decision == SessionWorkReleaseAdmission::AlreadyReleased,
            identity && stopped && epoch && cleared
        );
    }

    #[kani::proof]
    fn stop_is_absorbing_for_every_work_state() {
        let state = symbolic_state(kani::any());
        assert_eq!(state.after_stop(), WorkState::Stopped);
        assert_eq!(state.after_stop().after_stop(), WorkState::Stopped);
    }

    #[kani::proof]
    fn first_heartbeat_is_authorized_exactly_once() {
        let first = HeartbeatCondition::First;
        assert!(first.permits(None));
        assert!(!first.permits(Some("already-recorded")));
    }

    #[kani::proof]
    fn matching_heartbeat_rejects_every_other_receipt() {
        let matching = HeartbeatCondition::Matching("expected".into());
        assert!(matching.permits(Some("expected")));
        assert!(!matching.permits(None));
        assert!(!matching.permits(Some("stale")));
    }
}

#[cfg(kani)]
#[kani::proof]
fn session_utc_projection_domain_is_total_exact_and_idempotent() {
    // D1 negative -> epoch; D2 legal -> exact; D3 overflow -> last UTC instant.
    // Arbitrary i128 exhausts both boundaries. This proves the production
    // projection clamp, not the OS clock, calendar codec, or database adapter.
    let input: i128 = kani::any();
    let value = bounded_rfc3339_nanos(input);
    assert!((0..=MAX_RFC3339_NANOS).contains(&value));
    assert_eq!(bounded_rfc3339_nanos(value), value);
    if input < 0 {
        assert_eq!(value, 0);
    } else if input > MAX_RFC3339_NANOS {
        assert_eq!(value, MAX_RFC3339_NANOS);
    } else {
        assert_eq!(value, input);
    }
}
