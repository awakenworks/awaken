//! The Deployment domain's 5-field POSIX cron schedule value object:
//! `minute hour day-of-month
//! month day-of-week`. It validates an expression at create/update (a malformed
//! schedule is rejected, not silently stored), matches a wall-clock instant, and
//! computes the next occurrence that drives the timed-trigger firing.
//!
//! Scope: standard fields with `*`, ranges (`a-b`), lists (`a,b`), and steps
//! (`*/n`, `a-b/n`); day-of-week `0`/`7` both mean Sunday; the classic dom∧dow
//! quirk (when BOTH are restricted, a match on EITHER fires). Extended syntax
//! (`L`, `W`, `#`, `?`, `@daily`, seconds/year) is unsupported, matching the SDK.
//! Every match is evaluated in the schedule's validated IANA timezone. UTC remains
//! a convenience wrapper for internal callers and tests; there is no second
//! timezone-blind scheduler.

use std::collections::BTreeSet;

use chrono::{Datelike, TimeZone, Timelike, Utc};
use chrono_tz::Tz;

const MS_PER_MIN: u64 = 60_000;
const MAX_CHRONO_TIMESTAMP_MS: u64 = i64::MAX as u64;

/// Project an epoch-millisecond instant onto the first whole-minute index that
/// is strictly later and still representable by chrono's signed timestamp.
/// Keeping this arithmetic in a small, total kernel makes the scheduler's
/// boundary behavior independently verifiable.
fn next_minute_index(after_ms: u64) -> Option<u64> {
    if after_ms > MAX_CHRONO_TIMESTAMP_MS {
        return None;
    }
    let minute = after_ms / MS_PER_MIN;
    let next = minute.checked_add(1)?;
    (next <= MAX_CHRONO_TIMESTAMP_MS / MS_PER_MIN).then_some(next)
}

fn minute_timestamp(minute: u64) -> Option<u64> {
    let timestamp = minute.checked_mul(MS_PER_MIN)?;
    (timestamp <= MAX_CHRONO_TIMESTAMP_MS).then_some(timestamp)
}

/// A parsed 5-field cron expression.
#[derive(Debug, Clone)]
pub struct Cron {
    minute: BTreeSet<u32>,
    hour: BTreeSet<u32>,
    dom: BTreeSet<u32>,
    month: BTreeSet<u32>,
    dow: BTreeSet<u32>,
    dom_restricted: bool,
    dow_restricted: bool,
}

impl Cron {
    /// Parse `expr`, returning a human-readable error for a malformed expression.
    pub fn parse(expr: &str) -> Result<Self, String> {
        let f: Vec<&str> = expr.split_whitespace().collect();
        if f.len() != 5 {
            return Err(format!(
                "cron expression must have 5 fields, got {}",
                f.len()
            ));
        }
        let mut dow = parse_field(f[4], 0, 7)?;
        // 7 and 0 both mean Sunday — normalize to 0.
        if dow.remove(&7) {
            dow.insert(0);
        }
        Ok(Self {
            minute: parse_field(f[0], 0, 59)?,
            hour: parse_field(f[1], 0, 23)?,
            dom: parse_field(f[2], 1, 31)?,
            month: parse_field(f[3], 1, 12)?,
            dow,
            dom_restricted: f[2] != "*",
            dow_restricted: f[4] != "*",
        })
    }

    /// True when `ts_ms` (epoch ms, UTC) falls on a scheduled minute.
    pub fn matches(&self, ts_ms: u64) -> bool {
        self.matches_in(ts_ms, Tz::UTC)
    }

    /// True when the instant's local wall clock in `timezone` matches this cron.
    pub fn matches_in(&self, ts_ms: u64, timezone: Tz) -> bool {
        let Some(instant) = Utc.timestamp_millis_opt(ts_ms as i64).single() else {
            return false;
        };
        let local = instant.with_timezone(&timezone);
        let (month, day, hour, minute, weekday) = (
            local.month(),
            local.day(),
            local.hour(),
            local.minute(),
            local.weekday().num_days_from_sunday(),
        );
        if !self.minute.contains(&minute)
            || !self.hour.contains(&hour)
            || !self.month.contains(&month)
        {
            return false;
        }
        let dom_ok = self.dom.contains(&day);
        let dow_ok = self.dow.contains(&weekday);
        // The POSIX quirk: with BOTH day fields restricted, either matching fires;
        // otherwise the restricted one (the other being `*`) must match.
        if self.dom_restricted && self.dow_restricted {
            dom_ok || dow_ok
        } else {
            dom_ok && dow_ok
        }
    }

    /// The next scheduled instant strictly after `after_ms`, searching up to ~366
    /// days ahead (bounded so an unsatisfiable expression terminates). `None` if
    /// none is found in the window.
    pub fn next_after(&self, after_ms: u64) -> Option<u64> {
        self.next_after_in(after_ms, Tz::UTC)
    }

    /// Next occurrence evaluated in the supplied IANA timezone. Iterating UTC
    /// instants naturally handles DST gaps and repeats without inventing local
    /// timestamps that never occur.
    pub fn next_after_in(&self, after_ms: u64, timezone: Tz) -> Option<u64> {
        // Start at the next whole minute boundary strictly after `after_ms`.
        let mut minute = next_minute_index(after_ms)?;
        let limit = minute.saturating_add(366 * 24 * 60);
        while minute < limit {
            let ts = minute_timestamp(minute)?;
            if self.matches_in(ts, timezone) {
                return Some(ts);
            }
            minute = minute.checked_add(1)?;
        }
        None
    }
}

#[cfg(kani)]
#[kani::proof]
fn minute_timestamp_projection_is_exact_and_bounded() {
    let minute: u64 = kani::any();
    kani::assume(minute <= MAX_CHRONO_TIMESTAMP_MS / MS_PER_MIN);

    let timestamp = minute_timestamp(minute).expect("supported minute is in-domain");
    assert_eq!(timestamp, minute * MS_PER_MIN);
    assert!(timestamp <= MAX_CHRONO_TIMESTAMP_MS);
}

#[cfg(kani)]
#[kani::proof]
fn minute_timestamp_projection_has_an_exact_input_domain() {
    let minute: u64 = kani::any();
    let projected = minute_timestamp(minute);

    assert_eq!(
        projected.is_some(),
        minute <= MAX_CHRONO_TIMESTAMP_MS / MS_PER_MIN
    );
    if let Some(timestamp) = projected {
        assert!(timestamp <= i64::MAX as u64);
    }
}

/// Parse one cron field (`*`, `a`, `a-b`, `a,b`, `*/n`, `a-b/n`) into its allowed
/// value set, bounded to `[min, max]`.
fn parse_field(spec: &str, min: u32, max: u32) -> Result<BTreeSet<u32>, String> {
    let mut out = BTreeSet::new();
    for part in spec.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (
                r,
                s.parse::<u32>()
                    .map_err(|_| format!("bad step in {part:?}"))?,
            ),
            None => (part, 1),
        };
        if step == 0 {
            return Err(format!("step cannot be zero in {part:?}"));
        }
        let (lo, hi) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            (
                a.parse::<u32>()
                    .map_err(|_| format!("bad range {part:?}"))?,
                b.parse::<u32>()
                    .map_err(|_| format!("bad range {part:?}"))?,
            )
        } else {
            let v = range
                .parse::<u32>()
                .map_err(|_| format!("bad value {part:?}"))?;
            (v, v)
        };
        if lo < min || hi > max || lo > hi {
            return Err(format!("{part:?} is out of range {min}-{max}"));
        }
        let mut v = lo;
        while v <= hi {
            out.insert(v);
            v += step;
        }
    }
    if out.is_empty() {
        return Err(format!("empty field {spec:?}"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-01-05 09:00:00 UTC is a Monday. (Sanity anchor for the matchers.)
    const MON_0900: u64 = 1_767_603_600_000;

    // Cause/effect graph and decision table for the schedule domain:
    // C1 field count/range/step valid, C2 instant matches minute/hour/month,
    // C3 day fields unrestricted/one restricted/both restricted, C4 timezone
    // transition. E1 parse succeeds/fails, E2 matches under POSIX DOM-or-DOW,
    // E3 next occurrence is strictly later and DST-safe. The tests below cover
    // invalid C1, every C3 branch, bounded E3, and DST gap/repeat behavior.

    #[test]
    fn rejects_malformed_expressions() {
        assert!(Cron::parse("* * *").is_err(), "too few fields");
        assert!(Cron::parse("0 9 * * 1-5 2026").is_err(), "too many fields");
        assert!(Cron::parse("60 * * * *").is_err(), "minute out of range");
        assert!(Cron::parse("0 24 * * *").is_err(), "hour out of range");
        assert!(Cron::parse("0 9 * * 8").is_err(), "dow out of range");
        assert!(Cron::parse("0 9 * * */0").is_err(), "zero step");
        assert!(Cron::parse("bad 9 * * *").is_err(), "non-numeric");
    }

    #[test]
    fn accepts_the_documented_examples() {
        assert!(Cron::parse("0 9 * * 1-5").is_ok(), "weekdays at 9am");
        assert!(Cron::parse("*/15 * * * *").is_ok(), "every 15 minutes");
        assert!(Cron::parse("0 0 1 1 *").is_ok(), "new year midnight");
        assert!(Cron::parse("0 9 * * 0").is_ok(), "sunday");
        assert!(Cron::parse("0 9 * * 7").is_ok(), "sunday as 7");
    }

    #[test]
    fn matches_weekday_nine_am() {
        let c = Cron::parse("0 9 * * 1-5").unwrap();
        assert!(c.matches(MON_0900), "monday 9am fires");
        assert!(!c.matches(MON_0900 + 60_000), "9:01 does not");
        assert!(!c.matches(MON_0900 + 5 * 86_400_000), "saturday does not");
    }

    #[test]
    fn dow_zero_and_seven_are_both_sunday() {
        let sunday_0900 = MON_0900 - 86_400_000; // 2026-01-04 is a Sunday
        assert!(Cron::parse("0 9 * * 0").unwrap().matches(sunday_0900));
        assert!(Cron::parse("0 9 * * 7").unwrap().matches(sunday_0900));
    }

    #[test]
    fn both_day_fields_restricted_fire_on_either_match() {
        // The POSIX quirk: `1st of month OR Monday` fires on both, not their AND.
        let c = Cron::parse("0 9 1 * 1").unwrap();
        assert!(c.matches(MON_0900), "a Monday (not the 1st) fires");
        let first_0900 = MON_0900 - 4 * 86_400_000; // 2026-01-01 09:00 (a Thursday)
        assert!(c.matches(first_0900), "the 1st (not a Monday) fires");
        let plain = MON_0900 + 86_400_000; // 2026-01-06, neither the 1st nor Monday
        assert!(!c.matches(plain), "a day that is neither does not fire");
    }

    #[test]
    fn an_unsatisfiable_schedule_yields_no_occurrence() {
        // Feb 30 never exists, so the bounded search returns None rather than
        // looping forever.
        let c = Cron::parse("0 0 30 2 *").unwrap();
        assert!(c.next_after(MON_0900).is_none());
    }

    #[test]
    fn rfc3339_round_trips_the_anchor() {
        assert_eq!(
            awaken_session_contract::epoch_millis_to_rfc3339(MON_0900),
            "2026-01-05T09:00:00Z"
        );
        assert_eq!(
            awaken_session_contract::epoch_millis_to_rfc3339(0),
            "1970-01-01T00:00:00Z"
        );
    }

    #[test]
    fn next_after_advances_to_the_following_occurrence() {
        let c = Cron::parse("*/15 * * * *").unwrap();
        assert_eq!(c.next_after(MON_0900).unwrap(), MON_0900 + 15 * 60_000);
        // Chaining from the returned instant walks the schedule forward.
        let second = c.next_after(MON_0900 + 15 * 60_000).unwrap();
        assert_eq!(second, MON_0900 + 30 * 60_000);
    }

    /// Timezone cause/effect decision table:
    /// | local rule | timezone | UTC instant | behavior |
    /// |---|---|---|---|
    /// | 09:00 | UTC | 09:00Z | match |
    /// | 09:00 | America/New_York (winter) | 14:00Z | match |
    /// | 09:00 | America/New_York (winter) | 09:00Z | reject |
    #[test]
    fn evaluates_the_same_expression_in_the_declared_iana_timezone() {
        let cron = Cron::parse("0 9 * * *").unwrap();
        let new_york: Tz = "America/New_York".parse().unwrap();
        assert!(cron.matches_in(MON_0900, Tz::UTC));
        assert!(cron.matches_in(MON_0900 + 5 * 60 * 60_000, new_york));
        assert!(!cron.matches_in(MON_0900, new_york));
        assert_eq!(
            cron.next_after_in(MON_0900, new_york),
            Some(MON_0900 + 5 * 60 * 60_000)
        );
    }

    #[test]
    fn dst_gap_is_skipped_and_repeated_wall_clock_fires_twice() {
        // DST cause/effect graph: C1 `02:30` does not exist on New York's
        // 2026 spring-forward date; C2 `01:30` occurs once in EDT and once in
        // EST on the fall-back date. Effects: E1 C1 produces no invented March
        // 8 occurrence and advances to March 9; E2 C2 returns both distinct UTC
        // instants in order. Constraint K1 matching iterates real UTC minutes;
        // no local-time normalization or deduplication exists. Rules DST1=C1=>
        // E1; DST2=C2=>E2.
        let new_york: Tz = "America/New_York".parse().unwrap();
        let millis = |value: &str| {
            chrono::DateTime::parse_from_rfc3339(value)
                .unwrap()
                .timestamp_millis() as u64
        };

        let spring = Cron::parse("30 2 * * *").unwrap();
        assert_eq!(
            spring.next_after_in(millis("2026-03-08T05:00:00Z"), new_york),
            Some(millis("2026-03-09T06:30:00Z")),
            "DST1/E1"
        );

        let fall = Cron::parse("30 1 * * *").unwrap();
        let first = fall
            .next_after_in(millis("2026-11-01T04:00:00Z"), new_york)
            .expect("DST2 first 01:30");
        let second = fall
            .next_after_in(first, new_york)
            .expect("DST2 repeated 01:30");
        assert_eq!(first, millis("2026-11-01T05:30:00Z"), "DST2/E2 EDT");
        assert_eq!(second, millis("2026-11-01T06:30:00Z"), "DST2/E2 EST");
    }
}
