//! The environment work-queue port + its neutral vocabulary (ADR self-hosted work).
//!
//! The port the environments work-queue routes drive, plus the neutral domain shapes
//! in its signatures. The in-memory reference backend and the shared lease
//! bookkeeping live outward in `awaken-work-store`, beside the sqlite/postgres
//! siblings. The Managed wire adapter owns the neutral→wire projection
//! (`WorkItem` → `BetaSelfHostedWork`); this crate names no wire type.

use std::collections::BTreeMap;

use async_trait::async_trait;

/// The frozen object timestamp the managed wire uses (single-machine builds have
/// no real clock in the *projection*; wire timestamps carry presence, not wall
/// time). Real wall time enters only as the `now_ms` argument the routes pass to
/// the lease/poll bookkeeping — never onto the wire — so the wire shape is
/// unchanged while leases can expire and pollers can be counted.
pub const OBJECT_AT: &str = "2026-01-01T00:00:00Z";

/// Produce the opaque RFC-3339 receipt returned by a successful heartbeat.
///
/// Repeated writes in the same millisecond still receive distinct tokens, and a
/// wall clock that moves backwards cannot repeat an earlier receipt. Keeping this
/// pure conversion in the neutral queue contract lets every backend share the
/// exact compare-and-set vocabulary without importing a clock/date crate.
#[must_use]
pub fn next_heartbeat_receipt(now_ms: u64, previous: Option<&str>) -> String {
    const NANOS_PER_MILLI: i128 = 1_000_000;
    const MAX_RFC3339_NANOS: i128 = 253_402_300_799_999_999_999;

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

fn parse_rfc3339_utc_nanos(value: &str) -> Option<i128> {
    let body = value.strip_suffix('Z')?;
    let (whole, fraction) = body.split_once('.').map_or((body, ""), |parts| parts);
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

fn format_rfc3339_utc_nanos(total_nanos: i128) -> String {
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
    pub state: &'static str,
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

/// The port the environments work-queue routes drive. In-memory by default; a
/// durable impl (sqlite / postgres) backs it at parity. Membership is enforced by
/// the port: an operation on a `wid` that does not belong to `env_id` returns
/// `None`, which the route maps to a `work not found` 404.
#[async_trait]
pub trait WorkQueue: Send + Sync {
    /// Enqueue a `session` work item; returns the new work id.
    async fn enqueue_session(&self, env_id: &str, session_id: &str) -> String;
    /// Seed a `healthcheck` work item (its inner id is the work id); returns it.
    async fn enqueue_healthcheck(&self, env_id: &str) -> String;
    /// All work items in `env_id`, ascending by id (enqueue order).
    async fn list(&self, env_id: &str) -> Vec<WorkItem>;
    /// The work item under `wid` when it belongs to `env_id`.
    async fn get(&self, env_id: &str, wid: &str) -> Option<WorkItem>;
    /// Poll as `worker_id` at wall time `now_ms`: first reclaim any `active` item
    /// whose lease has lapsed (its worker went away), then lease the oldest queued
    /// item (queued→active) when none is actively leased. `None` when the queue is
    /// empty or one is still live-leased. The poll is recorded for `workers_polling`.
    async fn claim(&self, env_id: &str, worker_id: &str, now_ms: u64) -> Option<WorkItem>;
    /// Acknowledge receipt (queued→starting), stamping `acknowledged_at`.
    async fn ack(&self, env_id: &str, wid: &str) -> Option<WorkItem>;
    /// Atomically compare the preceding heartbeat and, when it matches, record a
    /// new heartbeat at `now_ms` and extend the lease.
    async fn heartbeat(
        &self,
        env_id: &str,
        wid: &str,
        worker_id: &str,
        now_ms: u64,
        heartbeat: LeaseHeartbeat,
    ) -> HeartbeatResult;
    /// Request a stop (→stopped).
    async fn stop(&self, env_id: &str, wid: &str) -> Option<WorkItem>;
    /// Merge a metadata patch (each present key upserts).
    async fn update_metadata(
        &self,
        env_id: &str,
        wid: &str,
        patch: BTreeMap<String, String>,
    ) -> Option<WorkItem>;
    /// Queue stats for `env_id` as of `now_ms` (for the `workers_polling` window).
    async fn stats(&self, env_id: &str, now_ms: u64) -> QueueStats;
    /// Drop all work for `env_id` (on environment delete).
    async fn remove_env(&self, env_id: &str);
}

#[cfg(test)]
mod tests {
    use super::next_heartbeat_receipt;

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
}

#[cfg(kani)]
mod verification {
    use super::*;

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
